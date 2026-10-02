// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::Write;

use rust_xlsxwriter::{Color, DocProperties, Format, FormatAlign, Workbook, XlsxError};
use strum::IntoEnumIterator;

use super::{ReportableCrate, common};
use crate::Result;
use crate::expr::{Appraisal, Risk};
use crate::metrics::{MetricCategory, MetricValue};
use crate::reports::common::ReportContext;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GenerationSite {
    Header,
    FreezePanes,
    Appraisal,
    ReasonsLabel,
    Reasons,
    CategoryHeader,
    CategoryFill,
    MetricName,
    MetricValue,
}

#[cfg(test)]
thread_local! {
    static FAIL_AT_SITE: core::cell::Cell<Option<GenerationSite>> = const { core::cell::Cell::new(None) };
}

pub fn generate<W: Write>(crates: &[ReportableCrate], writer: &mut W) -> Result<()> {
    let ctx = ReportContext::new(crates);
    generate_with_context(crates, &ctx, writer)
}

#[expect(unused_results, reason = "rust_xlsxwriter methods return &mut Worksheet for chaining")]
fn generate_with_context<W: Write>(crates: &[ReportableCrate], ctx: &ReportContext<'_>, writer: &mut W) -> Result<()> {
    let mut workbook = Workbook::new();

    // Set document properties
    let properties = DocProperties::new().set_author("cargo-aprz");
    workbook.set_properties(&properties);

    let worksheet = workbook
        .add_worksheet()
        .set_name("Crate Metrics")
        .expect("the fixed worksheet name is valid and shorter than Excel's 31-character limit");

    // Create formats
    let bold_format = Format::new().set_bold();
    let category_format = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0x00FE_D7AA))
        .set_align(FormatAlign::Left);
    let left_align_format = Format::new().set_align(FormatAlign::Left);
    let low_risk_format = Format::new()
        .set_background_color(Color::RGB(0x00C8_E6C9))
        .set_font_color(Color::RGB(0x002E_7D32))
        .set_bold();
    let medium_risk_format = Format::new()
        .set_background_color(Color::RGB(0x00FF_F9C4))
        .set_font_color(Color::RGB(0x00F5_7F17))
        .set_bold();
    let high_risk_format = Format::new()
        .set_background_color(Color::RGB(0x00FF_CDD2))
        .set_font_color(Color::RGB(0x00C6_2828))
        .set_bold();

    // Write crate names as column headers (starting from column B)
    for (col_idx, crate_info) in crates.iter().enumerate() {
        let header = format!("{} v{}", crate_info.name, crate_info.version);
        #[expect(clippy::cast_possible_truncation, reason = "Column index limited by Excel's u16 column limit")]
        propagate_xlsx_at(
            GenerationSite::Header,
            worksheet.write_string_with_format(0, (col_idx + 1) as u16, &header, &bold_format),
        )?;
    }

    // Freeze the first column (metric names) and first row (headers)
    propagate_xlsx_at(GenerationSite::FreezePanes, worksheet.set_freeze_panes(1, 1))?;

    // Write metrics as rows, grouped by category
    let mut row = 1;

    // Add appraisal rows if any crate has one
    let has_appraisals = crates.iter().any(|c| c.appraisal.is_some());
    if has_appraisals {
        // Result row with colored cells
        worksheet
            .write_string_with_format(row, 0, "Appraisals", &bold_format)
            .expect("the fixed appraisal label and bounded initial row and column are valid");
        for (col_idx, crate_info) in crates.iter().enumerate() {
            if let Some(eval) = &crate_info.appraisal {
                let (value, _) = appraisal_cell_values(eval);
                let format = match eval.risk() {
                    Risk::Low => &low_risk_format,
                    Risk::Medium => &medium_risk_format,
                    Risk::High => &high_risk_format,
                };
                #[expect(clippy::cast_possible_truncation, reason = "Column index limited by Excel's u16 column limit")]
                propagate_xlsx_at(
                    GenerationSite::Appraisal,
                    worksheet.write_string_with_format(row, (col_idx + 1) as u16, value, format),
                )?;
            }
        }
        row += 1;

        // Reasons row
        propagate_xlsx_at(
            GenerationSite::ReasonsLabel,
            worksheet.write_string_with_format(row, 0, "Reasons", &bold_format),
        )?;
        propagate_report_at(
            GenerationSite::Reasons,
            write_eval_row(worksheet, row, crates, |eval| appraisal_cell_values(eval).1),
        )?;
        row += 1;

        // Add blank row after evaluation
        row += 1;
    }

    // Write metrics grouped by category
    for category in MetricCategory::iter() {
        if let Some(category_metric_names) = ctx.metrics_by_category.get(&category) {
            // Write category header (uppercase and bold with background color)
            propagate_xlsx_at(
                GenerationSite::CategoryHeader,
                worksheet.write_string_with_format(row, 0, category.as_uppercase_str(), &category_format),
            )?;

            // Fill the rest of the category row with the same background color
            #[expect(clippy::cast_possible_truncation, reason = "Column count is limited by Excel's u16 column limit")]
            for c in 1..=crates.len() as u16 {
                propagate_xlsx_at(GenerationSite::CategoryFill, worksheet.write_blank(row, c, &category_format))?;
            }

            row += 1;

            // Write each metric in this category
            for &metric_name in category_metric_names {
                propagate_xlsx_at(GenerationSite::MetricName, worksheet.write_string(row, 0, metric_name))?;

                // Write values for each crate
                for (col_idx, metric_map) in ctx.crate_metric_maps.iter().enumerate() {
                    if let Some(metric) = metric_map.get(metric_name)
                        && let Some(ref value) = metric.value
                    {
                        #[expect(clippy::cast_possible_truncation, reason = "Column index limited by Excel's u16 column limit")]
                        let result = write_metric_value(worksheet, row, (col_idx + 1) as u16, metric_name, value, &left_align_format);
                        propagate_report_at(GenerationSite::MetricValue, result)?;
                    }
                }
                row += 1;
            }

            // Add blank row after category
            row += 1;
        }
    }

    // Auto-fit all columns
    worksheet.autofit();

    // Write workbook to output
    let data = workbook
        .save_to_buffer()
        .expect("saving a validated workbook to its in-memory buffer cannot incur an I/O failure");
    propagate_io(writer.write_all(&data))?;

    Ok(())
}

fn propagate_xlsx<T>(result: core::result::Result<T, XlsxError>) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => Err(error.into()),
    }
}

fn propagate_xlsx_at<T>(site: GenerationSite, result: core::result::Result<T, XlsxError>) -> Result<T> {
    #[cfg(test)]
    if FAIL_AT_SITE.with(|fail_at| fail_at.get() == Some(site)) {
        return Err(XlsxError::RowColumnLimitError.into());
    }
    #[cfg(not(test))]
    let _ = site;

    propagate_xlsx(result)
}

fn propagate_report<T>(result: Result<T>) -> Result<T> {
    result
}

fn propagate_report_at<T>(site: GenerationSite, result: Result<T>) -> Result<T> {
    #[cfg(test)]
    if FAIL_AT_SITE.with(|fail_at| fail_at.get() == Some(site)) {
        return Err(ohno::app_err!("injected report-generation failure"));
    }
    #[cfg(not(test))]
    let _ = site;

    propagate_report(result)
}

fn propagate_io<T>(result: std::io::Result<T>) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => Err(error.into()),
    }
}

#[expect(unused_results, reason = "rust_xlsxwriter methods return &mut Worksheet for chaining")]
#[expect(clippy::cast_precision_loss, reason = "Intentional conversion to f64 for Excel output")]
fn write_metric_value(
    worksheet: &mut rust_xlsxwriter::Worksheet,
    row: u32,
    col: u16,
    metric_name: &str,
    value: &MetricValue,
    format: &Format,
) -> Result<()> {
    match value {
        MetricValue::UInt(u) => {
            worksheet.write_number_with_format(row, col, *u as f64, format)?;
        }
        MetricValue::Float(f) => {
            worksheet.write_number_with_format(row, col, *f, format)?;
        }
        MetricValue::Boolean(b) => {
            worksheet.write_boolean_with_format(row, col, *b, format)?;
        }
        MetricValue::String(s) => {
            // Check if this is a URL
            if common::is_url(s.as_str()) {
                worksheet.write_url(row, col, s.as_str())?;
            }
            // Check if this is keywords or categories
            else if common::is_keywords_metric(metric_name) || common::is_categories_metric(metric_name) {
                // For keywords/categories, format with # prefix
                let formatted = common::format_keywords_or_categories_with_prefix(s.as_str());
                worksheet.write_string_with_format(row, col, formatted, format)?;
            } else {
                worksheet.write_string_with_format(row, col, s.as_str(), format)?;
            }
        }
        MetricValue::DateTime(dt) => {
            worksheet.write_string_with_format(row, col, dt.format("%Y-%m-%d").to_string(), format)?;
        }
        MetricValue::List(_) => {
            // Format list as comma-separated string
            let formatted = common::format_metric_value(value);
            worksheet.write_string_with_format(row, col, formatted, format)?;
        }
    }
    Ok(())
}

fn appraisal_cell_values(appraisal: &Appraisal) -> (String, String) {
    (
        common::format_appraisal_status(appraisal),
        common::join_with(appraisal.expression_outcomes.iter().map(common::outcome_icon_name), "; "),
    )
}

/// Helper function to write a evaluation row (Status or Reasons)
#[expect(unused_results, reason = "rust_xlsxwriter methods return &mut Worksheet for chaining")]
fn write_eval_row<F>(worksheet: &mut rust_xlsxwriter::Worksheet, row: u32, crates: &[ReportableCrate], extract_value: F) -> Result<()>
where
    F: Fn(&Appraisal) -> String,
{
    for (col_idx, crate_info) in crates.iter().enumerate() {
        if let Some(eval) = &crate_info.appraisal {
            let value = extract_value(eval);
            #[expect(clippy::cast_possible_truncation, reason = "Column index limited by Excel's u16 column limit")]
            worksheet.write_string(row, (col_idx + 1) as u16, &value)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::io::Read as _;
    use std::sync::Arc;

    use flate2::read::DeflateDecoder;

    use super::*;
    use crate::expr::{ExpressionDisposition, ExpressionOutcome};
    use crate::metrics::{Metric, MetricDef};

    static NAME_DEF: MetricDef = MetricDef {
        name: "name",
        description: "Crate name",
        category: MetricCategory::Metadata,
        extractor: |_| None,
        default_value: || None,
    };

    static VERSION_DEF: MetricDef = MetricDef {
        name: "version",
        description: "Crate version",
        category: MetricCategory::Metadata,
        extractor: |_| None,
        default_value: || None,
    };

    static STABILITY_DEF: MetricDef = MetricDef {
        name: "stability.score",
        description: "Stability score",
        category: MetricCategory::Stability,
        extractor: |_| None,
        default_value: || None,
    };

    fn create_test_crate(name: &str, version: &str, evaluation: Option<Appraisal>) -> ReportableCrate {
        let metrics = vec![
            Metric::with_value(&NAME_DEF, MetricValue::String(name.into())),
            Metric::with_value(&VERSION_DEF, MetricValue::String(version.into())),
        ];
        ReportableCrate::new(name.into(), Arc::new(version.parse().unwrap()), metrics, evaluation)
    }

    fn read_u16(bytes: &[u8], offset: usize) -> usize {
        usize::from(u16::from_le_bytes(
            bytes
                .get(offset..offset + 2)
                .expect("test workbook zip field is in bounds")
                .try_into()
                .expect("slice length is exactly two bytes"),
        ))
    }

    fn read_u32(bytes: &[u8], offset: usize) -> usize {
        u32::from_le_bytes(
            bytes
                .get(offset..offset + 4)
                .expect("test workbook zip field is in bounds")
                .try_into()
                .expect("slice length is exactly four bytes"),
        ) as usize
    }

    fn xlsx_entry(bytes: &[u8], entry_name: &str) -> String {
        let mut offset = 0;
        while offset + 46 <= bytes.len() {
            if bytes[offset..offset + 4] != [0x50, 0x4b, 0x01, 0x02][..] {
                offset += 1;
                continue;
            }

            let method = read_u16(bytes, offset + 10);
            let compressed_size = read_u32(bytes, offset + 20);
            let name_len = read_u16(bytes, offset + 28);
            let extra_len = read_u16(bytes, offset + 30);
            let comment_len = read_u16(bytes, offset + 32);
            let local_header_offset = read_u32(bytes, offset + 42);
            let name_start = offset + 46;
            let name_end = name_start + name_len;
            let name = core::str::from_utf8(bytes.get(name_start..name_end).expect("central directory filename is in bounds"))
                .expect("xlsx entry names are UTF-8");

            if name == entry_name {
                assert_eq!(
                    bytes
                        .get(local_header_offset..local_header_offset + 4)
                        .expect("local file header signature is in bounds"),
                    &[0x50, 0x4b, 0x03, 0x04][..]
                );
                let local_name_len = read_u16(bytes, local_header_offset + 26);
                let local_extra_len = read_u16(bytes, local_header_offset + 28);
                let data_start = local_header_offset + 30 + local_name_len + local_extra_len;
                let data_end = data_start + compressed_size;
                let data = bytes.get(data_start..data_end).expect("xlsx entry data is in bounds");
                let mut decoded = Vec::new();
                match method {
                    0 => decoded.extend_from_slice(data),
                    8 => {
                        DeflateDecoder::new(data)
                            .read_to_end(&mut decoded)
                            .expect("worksheet XML deflates successfully");
                    }
                    other => panic!("unsupported xlsx compression method {other}"),
                }
                return String::from_utf8(decoded).expect("xlsx XML is UTF-8");
            }

            offset = name_end + extra_len + comment_len;
        }

        panic!("xlsx entry {entry_name} was not found");
    }

    fn decode_xml_text(text: &str) -> String {
        text.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    fn shared_strings(workbook: &[u8]) -> Vec<String> {
        let xml = xlsx_entry(workbook, "xl/sharedStrings.xml");
        let mut strings = Vec::new();
        let mut rest = xml.as_str();
        while let Some(t_start) = rest.find("<t") {
            let after_t = &rest[t_start..];
            let Some(content_start) = after_t.find('>').map(|i| i + 1) else {
                break;
            };
            let after_content_start = &after_t[content_start..];
            let Some(content_end) = after_content_start.find("</t>") else {
                break;
            };
            strings.push(decode_xml_text(&after_content_start[..content_end]));
            rest = &after_content_start[content_end + "</t>".len()..];
        }
        strings
    }

    fn sheet_cell_text(workbook: &[u8], cell: &str) -> Option<String> {
        let sheet = xlsx_entry(workbook, "xl/worksheets/sheet1.xml");
        let shared_strings = shared_strings(workbook);
        let needle = format!("r=\"{cell}\"");
        for cell_xml in sheet.split("<c ").skip(1) {
            let tag_end = cell_xml.find('>')?;
            let tag = &cell_xml[..tag_end];
            if !tag.contains(&needle) {
                continue;
            }
            let cell_end = cell_xml.find("</c>").unwrap_or(tag_end);
            let body = &cell_xml[tag_end + 1..cell_end];
            if tag.contains("t=\"s\"") {
                let value_start = body.find("<v>")? + "<v>".len();
                let value_end = body[value_start..].find("</v>")? + value_start;
                let index: usize = body[value_start..value_end].parse().expect("shared string index is numeric");
                return Some(shared_strings[index].clone());
            }
            if tag.contains("t=\"b\"") {
                let value_start = body.find("<v>")? + "<v>".len();
                let value_end = body[value_start..].find("</v>")? + value_start;
                return Some(body[value_start..value_end].to_owned());
            }
            if let Some(value_start) = body.find("<v>").map(|i| i + "<v>".len()) {
                let value_end = body[value_start..].find("</v>")? + value_start;
                return Some(body[value_start..value_end].to_owned());
            }
            if let Some(text_start) = body.find("<t>").map(|i| i + "<t>".len()) {
                let text_end = body[text_start..].find("</t>")? + text_start;
                return Some(decode_xml_text(&body[text_start..text_end]));
            }
            return None;
        }
        None
    }

    fn sheet_cell_xml(workbook: &[u8], cell: &str) -> Option<String> {
        let sheet = xlsx_entry(workbook, "xl/worksheets/sheet1.xml");
        let needle = format!("r=\"{cell}\"");
        sheet
            .split("<c ")
            .skip(1)
            .find(|cell_xml| cell_xml.split_once('>').is_some_and(|(tag, _)| tag.contains(&needle)))
            .map(|cell_xml| {
                let end = cell_xml.find("</c>").map_or(cell_xml.len(), |index| index + "</c>".len());
                format!("<c {}", &cell_xml[..end])
            })
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn test_generate_empty_crates() {
        let crates: Vec<ReportableCrate> = vec![];
        let mut output = Vec::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        // Should generate a valid Excel file (has content)
        assert!(!output.is_empty());
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn test_generate_single_crate_no_evaluation() {
        let crates = vec![create_test_crate("test_crate", "1.2.3", None)];
        let mut output = Vec::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        // Should generate a valid Excel file
        assert!(!output.is_empty());
        // Excel files start with PK (ZIP signature)
        assert_eq!(&output[0..2], b"PK");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn test_generate_single_crate_with_evaluation() {
        let eval = Appraisal::new(
            Risk::Low,
            vec![ExpressionOutcome::new("good".into(), "Good".into(), ExpressionDisposition::True)],
            1,
            1,
            100.0,
        );
        let crates = vec![create_test_crate("test_crate", "1.0.0", Some(eval))];
        let mut output = Vec::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        assert!(!output.is_empty());
        assert_eq!(&output[0..2], b"PK");
    }

    #[test]
    fn test_appraisal_cell_values_preserve_unscored_state_and_inconclusive_details() {
        let required = Appraisal::required_check_failure(vec![ExpressionOutcome::new(
            "Required facts".into(),
            "Facts must be available.".into(),
            ExpressionDisposition::Failed("service unavailable".into()),
        )]);

        let (status, reasons) = appraisal_cell_values(&required);

        assert_eq!(status, "HIGH RISK (1 required check inconclusive; weighted score not calculated)");
        assert_eq!(
            reasons,
            "➖ Required facts: Facts must be available. (failure to evaluate: service unavailable)"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn test_generate_multiple_crates() {
        let crates = vec![
            create_test_crate("crate_a", "1.0.0", None),
            create_test_crate("crate_b", "2.0.0", None),
        ];
        let mut output = Vec::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        assert!(!output.is_empty());
        // Verify it's a valid ZIP/Excel file
        assert_eq!(&output[0..2], b"PK");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn test_generate_denied_status() {
        let eval = Appraisal::new(
            Risk::High,
            vec![ExpressionOutcome::new(
                "security".into(),
                "Security issue".into(),
                ExpressionDisposition::False,
            )],
            1,
            0,
            0.0,
        );
        let crates = vec![create_test_crate("bad_crate", "1.0.0", Some(eval))];
        let mut output = Vec::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        assert!(!output.is_empty());
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn test_generate_with_missing_data() {
        let crates = vec![create_test_crate("missing", "1.0.0", None)];
        let mut output = Vec::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        // Should still generate valid file even with missing data
        assert!(!output.is_empty());
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn test_generate_mixed_found_and_missing() {
        let crates = vec![create_test_crate("good", "1.0.0", None), create_test_crate("bad", "1.0.0", None)];
        let mut output = Vec::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        assert!(!output.is_empty());
    }

    static KEYWORDS_DEF: MetricDef = MetricDef {
        name: "crate.keywords",
        description: "Crate keywords",
        category: MetricCategory::Metadata,
        extractor: |_| None,
        default_value: || None,
    };

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn test_generate_prefixes_keywords() {
        let metrics = vec![
            Metric::with_value(&NAME_DEF, MetricValue::String("serde".into())),
            Metric::with_value(&KEYWORDS_DEF, MetricValue::String("serialization, parsing".into())),
        ];
        let crates = vec![ReportableCrate::new(
            "serde".into(),
            Arc::new("1.0.0".parse().unwrap()),
            metrics,
            None,
        )];
        let mut output = Vec::new();

        generate(&crates, &mut output).unwrap();

        assert!(!output.is_empty(), "the workbook must be written");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn workbook_places_headers_appraisals_and_categories_in_expected_cells() {
        let appraisal = |label: &str, disposition| {
            Appraisal::new(
                Risk::Low,
                vec![ExpressionOutcome::new(label.into(), format!("{label} outcome").into(), disposition)],
                1,
                1,
                100.0,
            )
        };
        let crates = ["alpha", "beta", "gamma"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                let metrics = vec![
                    Metric::with_value(&NAME_DEF, MetricValue::String(name.into())),
                    Metric::with_value(&VERSION_DEF, MetricValue::String(format!("1.0.{index}").into())),
                    Metric::with_value(&STABILITY_DEF, MetricValue::UInt(80 + index as u64)),
                ];
                ReportableCrate::new(
                    name.into(),
                    Arc::new(format!("1.0.{index}").parse().unwrap()),
                    metrics,
                    Some(appraisal(name, ExpressionDisposition::True)),
                )
            })
            .collect::<Vec<_>>();
        let mut output = Vec::new();

        generate(&crates, &mut output).unwrap();

        assert_eq!(sheet_cell_text(&output, "B1").as_deref(), Some("alpha v1.0.0"));
        assert_eq!(sheet_cell_text(&output, "D1").as_deref(), Some("gamma v1.0.2"));
        assert_eq!(sheet_cell_text(&output, "A2").as_deref(), Some("Appraisals"));
        assert_eq!(
            sheet_cell_text(&output, "B2").as_deref(),
            Some("LOW RISK (score = 100, awarded points = 1, available points = 1)")
        );
        assert_eq!(sheet_cell_text(&output, "A3").as_deref(), Some("Reasons"));
        assert_eq!(sheet_cell_text(&output, "D3").as_deref(), Some("✔️ gamma"));
        assert_eq!(sheet_cell_text(&output, "A5").as_deref(), Some("METADATA"));
        assert_eq!(sheet_cell_text(&output, "A6").as_deref(), Some("name"));
        assert_eq!(sheet_cell_text(&output, "D6").as_deref(), Some("gamma"));
        assert_eq!(sheet_cell_text(&output, "A7").as_deref(), Some("version"));
        assert_eq!(sheet_cell_text(&output, "D7").as_deref(), Some("1.0.2"));
        assert_eq!(sheet_cell_text(&output, "A9").as_deref(), Some("STABILITY"));
        assert_eq!(sheet_cell_text(&output, "A10").as_deref(), Some("stability.score"));
        assert_eq!(sheet_cell_text(&output, "D10").as_deref(), Some("82"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn workbook_preserves_sheet_name_freeze_panes_and_category_fill_extent() {
        let crates = vec![
            create_test_crate("alpha", "1.0.0", None),
            create_test_crate("beta", "1.0.0", None),
            create_test_crate("gamma", "1.0.0", None),
        ];
        let mut output = Vec::new();
        generate(&crates, &mut output).unwrap();

        let workbook = xlsx_entry(&output, "xl/workbook.xml");
        assert!(workbook.contains(r#"name="Crate Metrics""#), "{workbook}");

        let sheet = xlsx_entry(&output, "xl/worksheets/sheet1.xml");
        assert!(sheet.contains(r#"<pane xSplit="1" ySplit="1""#), "{sheet}");
        assert!(sheet.contains("<cols>"), "autofit must emit column widths: {sheet}");
        for cell in ["B2", "C2", "D2"] {
            assert!(
                sheet_cell_xml(&output, cell).is_some_and(|xml| xml.contains(" s=")),
                "category fill must include {cell}: {sheet}"
            );
        }
        assert!(
            sheet_cell_xml(&output, "E2").is_none(),
            "category fill must stop after the last crate"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn workbook_uses_the_exact_report_palette() {
        let crates = vec![
            create_test_crate("low", "1.0.0", Some(Appraisal::new(Risk::Low, vec![], 1, 1, 100.0))),
            create_test_crate("medium", "1.0.0", Some(Appraisal::new(Risk::Medium, vec![], 2, 1, 50.0))),
            create_test_crate("high", "1.0.0", Some(Appraisal::new(Risk::High, vec![], 1, 0, 0.0))),
        ];
        let mut output = Vec::new();
        generate(&crates, &mut output).unwrap();

        let styles = xlsx_entry(&output, "xl/styles.xml");
        for rgb in ["FFFED7AA", "FFC8E6C9", "FF2E7D32", "FFFFF9C4", "FFF57F17", "FFFFCDD2", "FFC62828"] {
            assert!(styles.contains(&format!(r#"rgb="{rgb}""#)), "missing {rgb} from {styles}");
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn one_appraised_crate_is_enough_to_emit_appraisal_rows() {
        let crates = vec![
            create_test_crate("plain", "1.0.0", None),
            create_test_crate("scored", "1.0.0", Some(Appraisal::new(Risk::Low, vec![], 1, 1, 100.0))),
        ];
        let mut output = Vec::new();
        generate(&crates, &mut output).unwrap();

        assert_eq!(sheet_cell_text(&output, "A2").as_deref(), Some("Appraisals"));
        assert_eq!(
            sheet_cell_text(&output, "C2").as_deref(),
            Some("LOW RISK (score = 100, awarded points = 1, available points = 1)")
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn workbook_records_its_author() {
        let mut output = Vec::new();
        generate(&[], &mut output).unwrap();
        let properties = xlsx_entry(&output, "docProps/core.xml");
        assert!(properties.contains("<dc:creator>cargo-aprz</dc:creator>"), "{properties}");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn floating_point_metrics_are_written_on_their_metric_row() {
        let metrics = vec![Metric::with_value(&STABILITY_DEF, MetricValue::Float(12.5))];
        let crates = vec![ReportableCrate::new(
            "floaty".into(),
            Arc::new("1.0.0".parse().unwrap()),
            metrics,
            None,
        )];
        let mut output = Vec::new();
        generate(&crates, &mut output).unwrap();
        assert_eq!(sheet_cell_text(&output, "A3").as_deref(), Some("stability.score"));
        assert_eq!(sheet_cell_text(&output, "B3").as_deref(), Some("12.5"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn metric_value_kinds_are_written_to_the_requested_cells() {
        let created_at = chrono::DateTime::parse_from_rfc3339("2024-02-03T10:30:00Z").unwrap().to_utc();
        let values = [
            ("A1", "uint", MetricValue::UInt(7), "7"),
            ("B2", "float", MetricValue::Float(12.5), "12.5"),
            ("C3", "boolean", MetricValue::Boolean(true), "1"),
            (
                "D4",
                "url",
                MetricValue::String("https://example.invalid".into()),
                "https://example.invalid",
            ),
            ("E5", "crate.categories", MetricValue::String("web, cli".into()), "#web, #cli"),
            ("F6", "date", MetricValue::DateTime(created_at), "2024-02-03"),
            (
                "G7",
                "list",
                MetricValue::List(vec![MetricValue::String("one".into()), MetricValue::UInt(2)]),
                "one, 2",
            ),
        ];
        let mut workbook = Workbook::new();
        let worksheet = workbook.add_worksheet();
        let format = Format::new();

        for (index, (_, metric_name, value, _)) in values.iter().enumerate() {
            #[expect(clippy::cast_possible_truncation, reason = "The fixture has seven entries")]
            write_metric_value(worksheet, index as u32, index as u16, metric_name, value, &format).unwrap();
        }

        let output = workbook.save_to_buffer().unwrap();
        let sheet = xlsx_entry(&output, "xl/worksheets/sheet1.xml");
        assert!(
            sheet.contains(r#"<hyperlink ref="D4""#),
            "the URL cell must be an active hyperlink: {sheet}"
        );
        for (cell, _, _, expected) in values {
            assert_eq!(
                sheet_cell_text(&output, cell).as_deref(),
                Some(expected),
                "unexpected value in {cell}"
            );
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn workbook_writes_keyword_metric_values_with_hash_prefixes() {
        let metrics = vec![
            Metric::with_value(&NAME_DEF, MetricValue::String("serde".into())),
            Metric::with_value(&KEYWORDS_DEF, MetricValue::String("serialization, parsing".into())),
        ];
        let crates = vec![ReportableCrate::new(
            "serde".into(),
            Arc::new("1.0.0".parse().unwrap()),
            metrics,
            None,
        )];
        let mut output = Vec::new();

        generate(&crates, &mut output).unwrap();

        assert_eq!(sheet_cell_text(&output, "B4").as_deref(), Some("#serialization, #parsing"));
    }

    #[test]
    fn appraisal_reasons_use_semicolon_space_separators() {
        let appraisal = Appraisal::new(
            Risk::High,
            vec![
                ExpressionOutcome::new("first".into(), "First failed.".into(), ExpressionDisposition::False),
                ExpressionOutcome::new("second".into(), "Second failed.".into(), ExpressionDisposition::False),
            ],
            2,
            0,
            0.0,
        );

        assert_eq!(
            appraisal_cell_values(&appraisal).1,
            "❌ first: First failed.; ❌ second: Second failed."
        );
    }

    #[test]
    fn worksheet_helpers_propagate_out_of_range_errors() {
        let mut workbook = Workbook::new();
        let worksheet = workbook.add_worksheet();
        let format = Format::new();
        let created_at = chrono::DateTime::parse_from_rfc3339("2024-02-03T10:30:00Z").unwrap().to_utc();
        let values = [
            MetricValue::UInt(1),
            MetricValue::Float(1.5),
            MetricValue::Boolean(true),
            MetricValue::String("value".into()),
            MetricValue::DateTime(created_at),
            MetricValue::List(vec![MetricValue::UInt(1)]),
        ];

        for value in &values {
            assert!(
                write_metric_value(worksheet, u32::MAX, 0, "text", value, &format).is_err(),
                "{value:?} must propagate an invalid-row error"
            );
        }
        for (metric_name, value) in [
            ("url", MetricValue::String("https://example.invalid".into())),
            ("crate.categories", MetricValue::String("web, cli".into())),
        ] {
            assert!(
                write_metric_value(worksheet, u32::MAX, 0, metric_name, &value, &format).is_err(),
                "{metric_name} must propagate an invalid-row error"
            );
        }

        let crates = vec![create_test_crate(
            "scored",
            "1.0.0",
            Some(Appraisal::new(Risk::Low, vec![], 1, 1, 100.0)),
        )];
        assert!(write_eval_row(worksheet, u32::MAX, &crates, |_| "value".to_owned()).is_err());
        assert!(propagate_xlsx::<()>(Err(XlsxError::RowColumnLimitError)).is_err());
        assert!(propagate_report::<()>(Err(ohno::app_err!("synthetic report failure"))).is_err());
        assert!(propagate_io::<()>(Err(std::io::Error::other("synthetic I/O failure"))).is_err());
    }

    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("synthetic write failure"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime (rust_xlsxwriter)")]
    fn workbook_output_propagates_writer_errors() {
        assert!(generate(&[], &mut FailingWriter).is_err());
    }

    #[test]
    fn generation_propagates_each_injected_worksheet_failure() {
        let metrics = vec![
            Metric::with_value(&NAME_DEF, MetricValue::String("scored".into())),
            Metric::with_value(&STABILITY_DEF, MetricValue::UInt(80)),
        ];
        let crates = vec![ReportableCrate::new(
            "scored".into(),
            Arc::new("1.0.0".parse().unwrap()),
            metrics,
            Some(Appraisal::new(
                Risk::Low,
                vec![ExpressionOutcome::new(
                    "quality".into(),
                    "Quality is acceptable.".into(),
                    ExpressionDisposition::True,
                )],
                1,
                1,
                100.0,
            )),
        )];

        for site in [
            GenerationSite::Header,
            GenerationSite::FreezePanes,
            GenerationSite::Appraisal,
            GenerationSite::ReasonsLabel,
            GenerationSite::Reasons,
            GenerationSite::CategoryHeader,
            GenerationSite::CategoryFill,
            GenerationSite::MetricName,
            GenerationSite::MetricValue,
        ] {
            FAIL_AT_SITE.with(|fail_at| fail_at.set(Some(site)));
            let result = generate(&crates, &mut Vec::new());
            FAIL_AT_SITE.with(|fail_at| fail_at.set(None));
            assert!(result.is_err(), "failure at {site:?} must be returned");
        }
    }
}
