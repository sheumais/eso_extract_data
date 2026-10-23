//! LANG / CSV / TXT conversion helpers from EsoExtractData.cpp: `CreateIdMap`, `OutputLangEntryToFile`
//! and `DiffLangFiles`.

use eso_common::csv::CsvFile;
use eso_common::error::Result;
use eso_common::file::CFile;
use eso_common::lang::{output_lang_text, unescape_lang_text, EsoLangFile};
use eso_common::util::{string_ends_with_ignore_case, strtoul};
use eso_common::{cprintf, eso_error, print_error};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};

/// Packed id (`id | unknown << 32 | index << 42`) -> text.  A `BTreeMap` (the C++ used an unordered map)
/// makes the output files deterministic.
pub type LangIdMap = BTreeMap<u64, String>;

pub fn make_id64(id: u32, unknown: u32, index: u32) -> u64 {
    (id as u64).wrapping_add((unknown as u64) << 32).wrapping_add((index as u64) << 42)
}

/// `LoadSimpleTextFile`: one string per line.
pub fn load_simple_text_file(filename: &str) -> Result<Vec<String>> {
    let f = File::open(filename).map_err(|_| eso_error!("Error: Failed to open '{}' for input!", filename))?;
    Ok(BufReader::new(f).split(b'\n').map_while(|l| l.ok())
        .map(|mut l| { if l.last() == Some(&b'\r') { l.pop(); } String::from_utf8_lossy(&l).into_owned() })
        .collect())
}

pub fn create_id_map_from_lang(lang: &EsoLangFile) -> LangIdMap {
    lang.records().iter().map(|r| (make_id64(r.id, r.unknown, r.index), r.text.clone())).collect()
}

pub fn create_id_map_from_csv(csv: &CsvFile, use_po_csv_format: bool) -> LangIdMap {
    let mut map = LangIdMap::new();

    if let Some(first) = csv.data().first() {
        if use_po_csv_format && first.len() > 3 {
            print_error!("Warning: Expected CSV file to have 3 columns but it looks to only {}!", first.len());
        } else if !use_po_csv_format && first.len() < 5 {
            print_error!("Warning: Expected CSV file to have 5 columns but it looks to have only {}!", first.len());
        }
    }

    for row in csv.data().iter().skip(1) {
        let (id, value) = if use_po_csv_format {
            if row.len() < 2 { continue; }
            let id_str = &row[0];
            let Some(j1) = id_str.find('-') else { continue };
            let Some(j2) = id_str[j1 + 1..].find('-').map(|p| p + j1 + 1) else { continue };
            (make_id64(strtoul(id_str), strtoul(&id_str[j1 + 1..]), strtoul(&id_str[j2 + 1..])), &row[1])
        } else {
            if row.len() < 5 { continue; }
            (make_id64(strtoul(&row[0]), strtoul(&row[1]), strtoul(&row[2])), &row[4])
        };
        map.insert(id, unescape_lang_text(value));
    }
    map
}

/// Writes one row of a `.added.csv` / `.changed.csv` / `.removed.csv` file.
///
/// The C++ PO-format line had a stray `%` at the end of its first `Printf` format string (undefined
/// behaviour); it is dropped here.  Ids are printed signed (`%d`) like the original.
pub fn output_lang_entry_to_file(file: &mut CFile, id64: u64, text: &str, use_po_csv_format: bool, text1: &str, orig_text: &str) -> bool {
    let id = (id64 & 0xffff_ffff) as u32;
    let unknown = ((id64 >> 32) & 0x3ff) as u32;
    let index = ((id64 >> 42) & 0xfffff) as u32;
    let mut ok = true;

    if use_po_csv_format {
        ok &= cprintf!(file, "\"{}-{}-{}\",\"", id as i32, unknown as i32, index as i32);
    } else {
        ok &= cprintf!(file, "\"{}\",\"{}\",\"{}\",\"0\",\"", id as i32, unknown as i32, index as i32);
    }
    ok &= output_lang_text(file, text, true);
    ok &= cprintf!(file, "\",\"");
    ok &= output_lang_text(file, text1, true);
    ok &= cprintf!(file, "\",\"");
    ok &= output_lang_text(file, orig_text, true);
    ok &= cprintf!(file, "\"\n");
    ok
}

/// Loads one comparison input (LANG, CSV or TXT+ID file) into an id map.
fn load_id_map(filename: &str, id_filename: &str, label: &str, use_lang_text: bool, use_po: bool) -> Result<LangIdMap> {
    if string_ends_with_ignore_case(filename, ".lang") {
        let mut lang = EsoLangFile::new();
        lang.load(filename)?;
        Ok(create_id_map_from_lang(&lang))
    } else if string_ends_with_ignore_case(filename, ".csv") {
        let mut csv = CsvFile::new(!use_lang_text);
        csv.load(filename)?;
        Ok(create_id_map_from_csv(&csv, use_po))
    } else if string_ends_with_ignore_case(filename, ".txt") {
        if id_filename.is_empty() {
            return Err(eso_error!("Error: Missing ID file to go with TXT file '{}'!", filename));
        }
        print_error!("\tCreating {} LANG file from text file '{}' and ID file '{}'...", label, filename, id_filename);
        let text = load_simple_text_file(filename)?;
        let ids = load_simple_text_file(id_filename)?;
        let mut lang = EsoLangFile::new();
        lang.create_from_text(&text, &ids, use_po, false)?;
        Ok(create_id_map_from_lang(&lang))
    } else {
        Err(eso_error!("Error: Unknown file format for '{}' (expected LANG, TXT, or CSV)!", filename))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn diff_lang_files(
    orig_lang_filename: &str, orig_lang_id_filename: &str,
    filename1: &str, filename2: &str, id_filename1: &str, id_filename2: &str,
    output_filename: &str, use_lang_text: bool, use_po_csv_format: bool,
) -> Result<()> {
    print_error!("Performing LANG file difference on:");

    let mut map_orig = LangIdMap::new();
    if !orig_lang_filename.is_empty() {
        print_error!("\tOriginal Lang: {}", orig_lang_filename);
        map_orig = load_id_map(orig_lang_filename, orig_lang_id_filename, "original", use_lang_text, use_po_csv_format)?;
    }

    print_error!("\tOld: {}", filename1);
    let map1 = load_id_map(filename1, id_filename1, "old", use_lang_text, use_po_csv_format)?;

    print_error!("\tNew: {}", filename2);
    let map2 = load_id_map(filename2, id_filename2, "new", use_lang_text, use_po_csv_format)?;

    print_error!("\tFound {} strings in file #1.", map1.len());
    print_error!("\tFound {} strings in file #2.", map2.len());

    let added_name = format!("{}.added.csv", output_filename);
    let removed_name = format!("{}.removed.csv", output_filename);
    let changed_name = format!("{}.changed.csv", output_filename);

    let open = |name: &str| -> Result<CFile> {
        let mut f = CFile::new();
        f.open(name, "wb").map_err(|_| eso_error!("Error: Failed to open file '{}' for output!", name))?;
        Ok(f)
    };
    let mut added_file = open(&added_name)?;
    let mut removed_file = open(&removed_name)?;
    let mut changed_file = open(&changed_name)?;

    print_error!("Saving added text to '{}'...", added_name);
    print_error!("Saving changed text to '{}'...", changed_name);
    print_error!("Saving removed text to '{}'...", removed_name);

    let (mut add_count, mut diff_count, mut remove_count, mut kept_count) = (0usize, 0usize, 0usize, 0usize);
    let mut out_lang = EsoLangFile::new();

    for (&id, old_text) in &map1 {
        match map2.get(&id) {
            Some(new_text) => {
                if new_text != old_text {
                    diff_count += 1;
                    let orig = map_orig.get(&id).map(String::as_str).unwrap_or("");
                    output_lang_entry_to_file(&mut changed_file, id, new_text, use_po_csv_format, old_text, orig);
                    out_lang.add_entry64(id, new_text);
                } else if let Some(orig) = map_orig.get(&id) {
                    out_lang.add_entry64(id, orig);
                    kept_count += 1;
                } else {
                    out_lang.add_entry64(id, old_text);
                }
            }
            None => {
                remove_count += 1;
                output_lang_entry_to_file(&mut removed_file, id, old_text, use_po_csv_format, "", "");
            }
        }
    }

    for (&id, new_text) in &map2 {
        if !map1.contains_key(&id) {
            add_count += 1;
            output_lang_entry_to_file(&mut added_file, id, new_text, use_po_csv_format, "", "");
            out_lang.add_entry64(id, new_text);
        }
    }

    print_error!("\tAdditions = {}", add_count);
    print_error!("\tChanges   = {}", diff_count);
    print_error!("\tRemovals  = {}", remove_count);
    print_error!("\tKept      = {}", kept_count);

    out_lang.sort_records();

    if use_lang_text {
        let txt = format!("{}.txt", output_filename);
        out_lang.dump_text(&txt, use_po_csv_format).map_err(|_| eso_error!("Error: Failed to save new TXT file '{}'!", txt))?;
        print_error!("Saved new TXT file '{}'!", txt);

        let ids = format!("{}.id.txt", output_filename);
        out_lang.dump_text_id(&ids).map_err(|_| eso_error!("Error: Failed to save new ID file '{}'!", ids))?;
        print_error!("Saved new ID file '{}'!", ids);
    } else {
        let lang = format!("{}.lang", output_filename);
        out_lang.save(&lang).map_err(|_| eso_error!("Error: Failed to save new LANG file '{}'!", lang))?;
        print_error!("Saved new LANG file '{}'!", lang);
    }

    Ok(())
}
