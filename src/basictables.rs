use crate::fontinfo::*;
use crate::utils::adjust_offset;
use babelfont::OTScalar;
use fonttools::{
    cmap,
    font::{self, Font, Table},
    glyf,
    head::head,
    hhea, hmtx,
    name::{name, NameRecord, NameRecordID},
    os2::os2,
    post::post,
};
use std::cmp::{max, min};

use fonttools::utils::int_list_to_num;
use std::collections::BTreeMap;
use std::convert::TryInto;

// This takes a babelfont font, and creates most of the output fonttools-rs font.
pub fn fill_tables(
    input: &babelfont::Font,
    glyf_table: glyf::glyf,
    metrics: Vec<hmtx::Metric>,
    glyph_names: Vec<String>,
    codepoint_to_gid_mapping: BTreeMap<u32, u16>,
) -> Font {
    let mut font = Font::new(font::SfntVersion::TrueType);
    let head_table = compile_head(input, &glyf_table);
    let post_table = compile_post(input, &glyph_names);
    let os2_table = compile_os2(input, &metrics, &glyf_table, &codepoint_to_gid_mapping);
    let cmap_table = compile_cmap(codepoint_to_gid_mapping);
    let name_table = compile_name(input);
    let mut hhea_table = compile_hhea(input, &metrics, &glyf_table);

    // Serializing the hmtx table determines the number of "long" horizontal metrics,
    // (as sum glyphs can be stored *without* an advance width, only an LSB)
    // so we need to serialize it at this point so we can store that value in hhea.
    // See https://docs.microsoft.com/en-us/typography/opentype/spec/hmtx
    let hmtx_table = hmtx::hmtx { metrics };
    let (hmtx_bytes, num_h_metrics) = hmtx_table.to_bytes();
    hhea_table.numberOfHMetrics = num_h_metrics;

    let maxp_table = glyf_table.as_maxp10();

    font.tables.insert(*b"head", Table::Head(head_table));
    font.tables.insert(*b"hhea", Table::Hhea(hhea_table));
    font.tables.insert(*b"maxp", Table::Maxp(maxp_table));
    font.tables.insert(*b"OS/2", Table::Os2(os2_table));
    font.tables.insert(*b"hmtx", Table::Unknown(hmtx_bytes));
    font.tables.insert(*b"cmap", Table::Cmap(cmap_table));
    font.tables.insert(*b"glyf", Table::Glyf(glyf_table));
    font.tables.insert(*b"name", Table::Name(name_table));
    font.tables.insert(*b"post", Table::Post(post_table));

    // Don't worry, this will get filled in on `font.save`.
    font.tables.insert(*b"loca", Table::Unknown(vec![0]));

    font
}

pub fn compile_head(font: &babelfont::Font, glyf: &glyf::glyf) -> head {
    let mut minor = font.version.1;
    while minor > 999 {
        minor /= 10;
    }
    let font_revision: f32 = (font.version.0 as f32 * 1000.0 + minor as f32).round() / 1000.0;

    // bounding box
    let mut x_min: i16 = 0;
    let mut x_max: i16 = 0;
    let mut y_min: i16 = 0;
    let mut y_max: i16 = 0;
    for glyph in &glyf.glyphs {
        x_min = min(x_min, glyph.xMin);
        x_max = max(x_max, glyph.xMax);
        y_min = min(y_min, glyph.yMin);
        y_max = max(y_max, glyph.yMax);
    }
    let mut head_table = head::new(font_revision, font.upm, x_min, y_min, x_max, y_max);

    // dates (modified is set to now by default)
    head_table.created = font.date.naive_local();

    // XXX mac style

    if let Some(lowest_rec_ppm) = font.ot_value("head", "lowestRecPPEM", true) {
        head_table.lowestRecPPEM = u16::from(lowest_rec_ppm);
    }

    // // misc
    // if let Some(flags) = &info.open_type_head_flags {
    //     head_table.flags = int_list_to_num(flags) as u16;
    // }

    head_table
}

pub fn compile_post(font: &babelfont::Font, glyph_names: &[String]) -> post {
    let italic_angle = *font
        .default_master()
        .and_then(|x| x.metrics.get("italic angle"))
        .unwrap_or(&0) as f32;
    let underline_position = i16::from(
        font.ot_value("post", "underlinePosition", true)
            .unwrap_or_else(|| OTScalar::Float((font.upm as f32) * -0.075)),
    );
    let is_fixed_pitch = font
        .ot_value("post", "isFixedPitch", true)
        .map(bool::from)
        .unwrap_or(false);
    post::new(
        2.0,
        italic_angle,
        underline_position,
        postscript_underline_thickness(font), // in fontinfo
        is_fixed_pitch,
        Some(glyph_names.to_vec()),
    )
}

pub fn compile_cmap(codepoint_to_gid_mapping: BTreeMap<u32, u16>) -> cmap::cmap {
    // See which mappings cover the BMP
    let u16_mapping: BTreeMap<u32, u16> = codepoint_to_gid_mapping
        .iter()
        .filter(|&(k, _)| *k <= u16::MAX as u32)
        .map(|(k, v)| (*k, *v))
        .collect();
    let has_nonbmp = u16_mapping.len() < codepoint_to_gid_mapping.len();

    let mut subtables = vec![
        cmap::CmapSubtable {
            format: 4,
            platformID: 0,
            encodingID: 3,
            languageID: 0,
            mapping: u16_mapping.clone(),
        },
        cmap::CmapSubtable {
            format: 4,
            platformID: 3,
            encodingID: 1,
            languageID: 0,
            mapping: u16_mapping,
        },
    ];
    if has_nonbmp {
        subtables.push(cmap::CmapSubtable {
            format: 12,
            platformID: 0,
            encodingID: 3,
            languageID: 0,
            mapping: codepoint_to_gid_mapping.clone(),
        });
        subtables.push(cmap::CmapSubtable {
            format: 12,
            platformID: 3,
            encodingID: 1,
            languageID: 0,
            mapping: codepoint_to_gid_mapping,
        });
    }
    cmap::cmap { subtables }
}

#[allow(non_snake_case)]
pub fn compile_hhea(
    input: &babelfont::Font,
    metrics: &[hmtx::Metric],
    glyf: &glyf::glyf,
) -> hhea::hhea {
    let lineGap = input
        .ot_value("hhea", "lineGap", true)
        .map(i16::from)
        .unwrap_or(0);
    let caretOffset = input
        .ot_value("hhea", "caretOffset", true)
        .map(i16::from)
        .unwrap_or(0);
    let advanceWidthMax = metrics.iter().map(|x| x.advanceWidth).max().unwrap_or(0);
    let minLeftSideBearing = metrics.iter().map(|x| x.lsb).min().unwrap_or(0);
    let minRightSideBearing = metrics
        .iter()
        .map(|x| x.advanceWidth as i16)
        .zip(glyf.glyphs.iter().map(|g| g.xMax))
        .map(|t| t.0 - t.1)
        .min()
        .unwrap_or(0);
    let xMaxExtent = glyf.glyphs.iter().map(|g| g.xMax).max().unwrap_or(0);
    hhea::hhea {
        majorVersion: 1,
        minorVersion: 0,
        ascender: hhea_ascender(input),
        descender: hhea_descender(input),
        lineGap,
        advanceWidthMax,
        minLeftSideBearing,
        minRightSideBearing,
        xMaxExtent,
        caretSlopeRise: 1, // XXX
        caretSlopeRun: 0,  // XXX
        caretOffset,
        reserved0: 0,
        reserved1: 0,
        reserved2: 0,
        reserved3: 0,
        metricDataFormat: 0,
        numberOfHMetrics: 0,
    }
}

#[allow(non_snake_case)]
pub fn compile_os2(
    input: &babelfont::Font,
    metrics: &[hmtx::Metric],
    _glyf: &glyf::glyf,
    mapping: &BTreeMap<u32, u16>,
) -> os2 {
    let upm = input.upm as f64;
    let italic_angle = input.default_metric("italic angle").unwrap_or(0) as f64;

    // The fallback calculations here are all taken from ufo2ft

    let x_height = input
        .default_metric("xHeight")
        .unwrap_or((upm * 0.5) as i32);
    let font_ascender = ascender(input);
    let font_descender = descender(input);

    let sTypoAscender = input
        .ot_value("OS2", "sTypoAscender", true)
        .map(i16::from)
        .unwrap_or(font_ascender) as i16;
    let sTypoDescender = input
        .ot_value("OS2", "sTypoDescender", true)
        .map(i16::from)
        .unwrap_or_else(|| font_descender) as i16;
    let sTypoLineGap = input
        .ot_value("OS2", "sTypoLineGap", true)
        .map(i32::from)
        .unwrap_or((upm * 1.2) as i32 + (font_ascender - font_descender) as i32)
        as i16;

    let ySubscriptYOffset = input
        .ot_value("OS2", "ySubscriptYOffset", true)
        .map(i16::from)
        .unwrap_or((upm * 0.075).round() as i16);
    let ySubscriptXOffset = input
        .ot_value("OS2", "ySubscriptXOffset", true)
        .map_or_else(
            || adjust_offset(-ySubscriptYOffset, italic_angle),
            i32::from,
        ) as i16;

    let ySuperscriptYOffset = input
        .ot_value("OS2", "ySuperscriptYOffset", true)
        .map(i16::from)
        .unwrap_or((upm * 0.35).round() as i16);

    let ySuperscriptXOffset = input
        .ot_value("OS2", "ySuperscriptXOffset", true)
        .map_or_else(
            || adjust_offset(-ySuperscriptYOffset, italic_angle),
            i32::from,
        ) as i16;

    let ySubscriptXSize = input
        .ot_value("OS2", "ySubscriptXSize", true)
        .map_or((upm * 0.65).round() as i16, i16::from);
    let ySubscriptYSize = input
        .ot_value("OS2", "ySubscriptYSize", true)
        .map_or((upm * 0.6).round() as i16, i16::from);

    let ySuperscriptXSize = input
        .ot_value("OS2", "ySuperscriptXSize", true)
        .map_or((upm * 0.65).round() as i16, i16::from);
    let ySuperscriptYSize = input
        .ot_value("OS2", "ySuperscriptYSize", true)
        .map_or((upm * 0.6).round() as i16, i16::from);

    let xAvgCharWidth = (metrics.iter().map(|m| m.advanceWidth as f32).sum::<f32>()
        / metrics.iter().filter(|m| m.advanceWidth != 0).count() as f32)
        .round() as i16;
    let yStrikeoutSize = input
        .ot_value("OS2", "yStrikeoutSize", true)
        .map_or_else(|| postscript_underline_thickness(input).into(), i32::from)
        as i16;
    let yStrikeoutPosition = input
        .ot_value("OS2", "yStrikeoutPosition", true)
        .map_or((x_height as f32 * 0.22) as i16, i16::from);
    let achVendID = input
        .ot_value("OS2", "achVendID", true)
        .map_or(*b"NONE", |x| String::from(x).as_bytes().try_into().unwrap());
    let usWeightClass = input
        .ot_value("OS2", "usWeightClass", true)
        .map_or(400, i16::from) as u16;
    let usWidthClass = input
        .ot_value("OS2", "usWidthClass", true)
        .map_or(5, i16::from) as u16;
    let sCapHeight = Some(
        input
            .default_metric("cap height")
            .unwrap_or((upm * 0.7) as i32) as i16,
    );
    let usFirstCharIndex = *mapping.keys().min().unwrap_or(&0xFFFF) as u16;
    let usLastCharIndex = *mapping.keys().max().unwrap_or(&0xFFFF) as u16;
    let usWinAscent = input
        .ot_value("OS2", "usWinAscent", true)
        .map_or(font_ascender + sTypoLineGap, i16::from) as u16;
    let usWinDescent = input
        .ot_value("OS2", "usWinDescent", true)
        .map_or(font_descender.abs(), i16::from) as u16;
    let mut table = os2 {
        version: 4,
        xAvgCharWidth,
        usWeightClass,
        usWidthClass,
        // XXX OS2 fsType should be taken from babelfont, but check babelfont
        // actually sets it! Default should be zero?
        fsType: int_list_to_num(&[2]) as u16,
        ySubscriptXSize,
        ySubscriptYSize,
        ySubscriptYOffset,
        ySubscriptXOffset,

        ySuperscriptXSize,
        ySuperscriptYSize,
        ySuperscriptYOffset,
        ySuperscriptXOffset,

        yStrikeoutSize,
        yStrikeoutPosition,

        sxHeight: Some(x_height as i16),
        achVendID,
        sCapHeight,
        sTypoAscender,
        sTypoDescender,
        sTypoLineGap,
        usWinAscent,
        usWinDescent,
        usMaxContext: Some(0), // This should be changed later by the feature compiler
        usBreakChar: Some(32), // Yes, these are constants
        usDefaultChar: Some(0), // this too
        // sFamilyClass: input.open_type_os2_family_class... (not public)
        sFamilyClass: 0,
        panose: get_panose(input),
        ulCodePageRange1: Some(0),
        ulCodePageRange2: Some(0),
        ulUnicodeRange1: 0b10100001000000000000000011111111, // XXX
        ulUnicodeRange2: 0,                                  // XXX
        ulUnicodeRange3: 0,                                  // XXX
        ulUnicodeRange4: 0,                                  // XXX
        usFirstCharIndex,
        usLastCharIndex,
        usLowerOpticalPointSize: None,
        usUpperOpticalPointSize: None,
        fsSelection: get_selection(input),
    };
    if let Some(OTScalar::BitField(page_ranges)) = input.ot_value("OS2", "codePageRanges", true) {
        table.int_list_to_code_page_ranges(&page_ranges);
    } else {
        table.calc_code_page_ranges(&mapping);
    }
    table
}

pub fn compile_name(input: &babelfont::Font) -> name {
    let mut name = name { records: vec![] };
    /* Ideally...
    if let Some(records) = &input.open_type_name_records {
        for record in records {
            name.records.push(NameRecord {
                nameID: record.name_id as u16,
                platformID: record.platform_id as u16,
                encodingID: record.encoding_id as u16,
                languageID: record.language_id as u16,
                string: record.string,
            })
        }
    }
    */

    let mut records: Vec<(NameRecordID, String)> = vec![];
    if let Some(copyright) = &input.names.copyright.default() {
        records.push((NameRecordID::Copyright, copyright.to_string()));
    }

    let family_name = style_map_family_name(input);
    let style_name = style_map_style_name(input);
    let pfn = preferred_family_name(input);
    let psfn = preferred_subfamily_name(input);
    records.extend(vec![
        (NameRecordID::FontFamilyName, family_name.clone()),
        (NameRecordID::FontSubfamilyName, style_name.clone()),
        (NameRecordID::UniqueID, unique_id(input)),
        (NameRecordID::FullFontName, format!("{0} {1}", pfn, psfn)),
        (NameRecordID::Version, name_version(input)),
        (NameRecordID::PostscriptName, postscript_font_name(input)),
    ]);
    for (id, field) in &[
        (NameRecordID::Trademark, &input.names.trademark),
        (NameRecordID::Manufacturer, &input.names.manufacturer),
        (NameRecordID::Designer, &input.names.designer),
        (NameRecordID::Description, &input.names.description),
        (NameRecordID::ManufacturerURL, &input.names.manufacturer_url),
        (NameRecordID::DesignerURL, &input.names.designer_url),
        (NameRecordID::License, &input.names.license),
        (NameRecordID::LicenseURL, &input.names.license_url),
    ] {
        if let Some(value) = field.default() {
            records.push((*id, value.to_string()));
        }
    }

    if pfn != family_name {
        records.push((NameRecordID::PreferredFamilyName, pfn));
    }
    if psfn != style_name {
        records.push((NameRecordID::PreferredSubfamilyName, psfn));
    }

    for (id, field) in &[
        (
            NameRecordID::CompatibleFullName,
            &input.names.compatible_full_name,
        ),
        (NameRecordID::SampleText, &input.names.sample_text),
        // XXX PostScript CID findfont name ???
        (NameRecordID::WWSFamilyName, &input.names.w_w_s_family_name),
        (
            NameRecordID::WWSSubfamilyName,
            &input.names.w_w_s_subfamily_name,
        ),
    ] {
        if let Some(value) = field.default() {
            records.push((*id, value.to_string()));
        }
    }

    // XXX Light Background Palette.
    // XXX Dark Background Palette.
    // XXX Variations PostScript Name Prefix.

    for (id, string) in records {
        name.records.push(NameRecord::windows_unicode(id, string));
    }

    name
}
