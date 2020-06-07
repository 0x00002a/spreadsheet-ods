use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use chrono::{Duration, NaiveDate, NaiveDateTime};
use quick_xml::events::{BytesStart, Event};
use quick_xml::events::attributes::Attribute;
use zip::read::ZipFile;

use crate::{ColRange, RowRange, SCell, Sheet, ucell, Value, ValueFormat, ValueType, WorkBook, XMLOrigin};
use crate::error::OdsError;
use crate::format::{FormatPart, FormatPartType};
use crate::refs::parse_cellranges;
use crate::style::{FontDecl, Style, StyleFor};

// Reads an ODS-file.
pub fn read_ods<P: AsRef<Path>>(path: P) -> Result<WorkBook, OdsError> {
    read_ods_flags(path, false)
}

// Reads an ODS-file.
pub fn read_ods_flags<P: AsRef<Path>>(path: P, dump_xml: bool) -> Result<WorkBook, OdsError> {
    let file = File::open(path.as_ref())?;
    // ods is a zip-archive, we read content.xml
    let mut zip = zip::ZipArchive::new(file)?;
    let mut zip_file = zip.by_name("content.xml")?;

    let mut book = read_content(&mut zip_file, dump_xml)?;

    book.file = Some(path.as_ref().to_path_buf());

    Ok(book)
}

fn read_content(zip_file: &mut ZipFile, dump_xml: bool) -> Result<WorkBook, OdsError> {
    // xml parser
    let mut xml = quick_xml::Reader::from_reader(BufReader::new(zip_file));
    xml.trim_text(true);

    let mut buf = Vec::new();

    let mut book = WorkBook::new();
    let mut sheet = Sheet::new();

    // Separate counter for table-columns
    let mut tcol: ucell = 0;

    // Cell position
    let mut row: ucell = 0;
    let mut col: ucell = 0;

    // Rows can be repeated. In reality only empty ones ever are.
    let mut row_repeat: ucell = 1;
    // Row style.
    let mut row_style: Option<String> = None;

    let mut col_range_from = 0;
    let mut row_range_from = 0;

    loop {
        let event = xml.read_event(&mut buf)?;
        if dump_xml { log::debug!("{:?}", event); }
        match event {
            Event::Start(xml_tag)
            if xml_tag.name() == b"table:table" => {
                read_table(&xml, xml_tag, &mut sheet)?;
            }
            Event::End(xml_tag)
            if xml_tag.name() == b"table:table" => {
                row = 0;
                col = 0;
                book.push_sheet(sheet);
                sheet = Sheet::new();
            }

            Event::Start(xml_tag)
            if xml_tag.name() == b"table:table-header-columns" => {
                col_range_from = tcol;
            }

            Event::End(xml_tag)
            if xml_tag.name() == b"table:table-header-columns" => {
                sheet.header_cols = Some(ColRange::new(col_range_from, tcol - 1));
            }

            Event::Empty(xml_tag)
            if xml_tag.name() == b"table:table-column" => {
                tcol = read_table_column(&mut xml, &xml_tag, tcol, &mut sheet)?;
            }

            Event::Start(xml_tag)
            if xml_tag.name() == b"table:table-header-rows" => {
                row_range_from = row;
            }

            Event::End(xml_tag)
            if xml_tag.name() == b"table:table-header-rows" => {
                sheet.header_rows = Some(RowRange::new(row_range_from, row - 1));
            }

            Event::Start(xml_tag)
            if xml_tag.name() == b"table:table-row" => {
                row_repeat = read_table_row(&mut xml, xml_tag, &mut row_style)?;
            }
            Event::End(xml_tag)
            if xml_tag.name() == b"table:table-row" => {
                // There is often a strange repeat count for the last
                // row of the table that is in the millions.
                // That hits the break quite thoroughly, for now I ignore
                // this. Removes the row style for empty rows, I can live
                // with that for now.
                //
                // if let Some(style) = row_style {
                //     for r in row..row + row_repeat {
                //         sheet.set_row_style(r, style.clone());
                //     }
                // }
                row_style = None;

                row += row_repeat;
                col = 0;
                row_repeat = 1;
            }

            Event::Start(xml_tag)
            if xml_tag.name() == b"office:font-face-decls" =>
                read_fonts(&mut book, &mut xml, b"office:font-face-decls", dump_xml)?,

            Event::Start(xml_tag)
            if xml_tag.name() == b"office:automatic-styles" =>
                read_styles(&mut book, &mut xml, b"office:automatic-styles", dump_xml)?,

            Event::Empty(xml_tag)
            if xml_tag.name() == b"table:table-cell" || xml_tag.name() == b"table:covered-table-cell" => {
                col = read_empty_table_cell(&mut xml, xml_tag, row, col, &mut sheet)?;
            }

            Event::Start(xml_tag)
            if xml_tag.name() == b"table:table-cell" || xml_tag.name() == b"table:covered-table-cell" => {
                col = read_table_cell(&mut xml, xml_tag, row, col, &mut sheet, dump_xml)?;
            }

            Event::Eof => {
                break;
            }
            _ => {}
        }

        buf.clear();
    }

    Ok(book)
}

fn read_table(xml: &quick_xml::Reader<BufReader<&mut ZipFile>>,
              xml_tag: BytesStart,
              sheet: &mut Sheet) -> Result<(), OdsError> {
    for attr in xml_tag.attributes().with_checks(false) {
        match attr? {
            attr if attr.key == b"table:name" => {
                let v = attr.unescape_and_decode_value(xml)?;
                sheet.set_name(v);
            }
            attr if attr.key == b"table:style-name" => {
                let v = attr.unescape_and_decode_value(xml)?;
                sheet.set_style(v);
            }
            attr if attr.key == b"table:print-ranges" => {
                let v = attr.unescape_and_decode_value(xml)?;
                let mut pos = 0usize;
                sheet.print_ranges = parse_cellranges(v.as_str(), &mut pos)?;
            }
            _ => { /* ignore other attr */ }
        }
    }

    Ok(())
}

fn read_table_row(xml: &mut quick_xml::Reader<BufReader<&mut ZipFile>>,
                  xml_tag: BytesStart,
                  row_style: &mut Option<String>) -> Result<ucell, OdsError>
{
    let mut row_repeat: ucell = 1;

    for attr in xml_tag.attributes().with_checks(false) {
        match attr? {
            attr if attr.key == b"table:number-rows-repeated" => {
                let v = attr.unescaped_value()?;
                let v = xml.decode(v.as_ref())?;
                row_repeat = v.parse::<ucell>()?;
            }
            attr if attr.key == b"table:style-name" => {
                let v = attr.unescape_and_decode_value(&xml)?;
                *row_style = Some(v);
            }
            _ => { /* ignore other */ }
        }
    }

    Ok(row_repeat)
}

fn read_table_column(xml: &mut quick_xml::Reader<BufReader<&mut ZipFile>>,
                     xml_tag: &BytesStart,
                     mut tcol: ucell,
                     sheet: &mut Sheet) -> Result<ucell, OdsError> {
    let mut style = None;
    let mut cell_style = None;
    let mut repeat: ucell = 1;

    for attr in xml_tag.attributes().with_checks(false) {
        match attr? {
            attr if attr.key == b"table:style-name" => {
                let v = attr.unescape_and_decode_value(&xml)?;
                style = Some(v);
            }
            attr if attr.key == b"table:number-columns-repeated" => {
                let v = attr.unescape_and_decode_value(&xml)?;
                repeat = v.parse()?;
            }
            attr if attr.key == b"table:default-cell-style-name" => {
                let v = attr.unescape_and_decode_value(&xml)?;
                cell_style = Some(v);
            }
            _ => {}
        }
    }

    while repeat > 0 {
        if let Some(style) = &style {
            if sheet.col_style.is_none() {
                sheet.col_style = Some(BTreeMap::new());
            }
            if let Some(col_style) = &mut sheet.col_style {
                col_style.insert(tcol, style.clone());
            }
        }
        if let Some(cell_style) = &cell_style {
            if sheet.col_cell_style.is_none() {
                sheet.col_cell_style = Some(BTreeMap::new());
            }
            if let Some(col_cell_style) = &mut sheet.col_cell_style {
                col_cell_style.insert(tcol, cell_style.clone());
            }
        }
        tcol += 1;
        repeat -= 1;
    }

    Ok(tcol)
}

fn read_table_cell(xml: &mut quick_xml::Reader<BufReader<&mut ZipFile>>,
                   xml_tag: BytesStart,
                   row: ucell,
                   mut col: ucell,
                   sheet: &mut Sheet,
                   dump_xml: bool) -> Result<ucell, OdsError> {

    // Current cell tag
    let tag_name = xml_tag.name();

    // The current cell.
    let mut cell: SCell = SCell::new();
    // Columns can be repeated, not only empty ones.
    let mut cell_repeat: ucell = 1;
    // Decoded type.
    let mut value_type: Option<ValueType> = None;
    // Basic cell value here.
    let mut cell_value: Option<String> = None;
    // Content of the table-cell tag.
    let mut cell_content: Option<String> = None;
    // Currency
    let mut cell_currency: Option<String> = None;

    for attr in xml_tag.attributes().with_checks(false) {
        match attr? {
            attr if attr.key == b"table:number-columns-repeated" => {
                let v = attr.unescape_and_decode_value(&xml)?;
                cell_repeat = v.parse::<ucell>()?;
            }
            attr if attr.key == b"table:number-rows-spanned" => {
                let v = attr.unescape_and_decode_value(&xml)?;
                cell.span.0 = v.parse::<ucell>()?;
            }
            attr if attr.key == b"table:number-columns-spanned" => {
                let v = attr.unescape_and_decode_value(&xml)?;
                cell.span.1 = v.parse::<ucell>()?;
            }

            attr if attr.key == b"office:value-type" =>
                value_type = Some(decode_value_type(attr)?),

            attr if attr.key == b"office:date-value" =>
                cell_value = Some(attr.unescape_and_decode_value(&xml)?),
            attr if attr.key == b"office:time-value" =>
                cell_value = Some(attr.unescape_and_decode_value(&xml)?),
            attr if attr.key == b"office:value" =>
                cell_value = Some(attr.unescape_and_decode_value(&xml)?),
            attr if attr.key == b"office:boolean-value" =>
                cell_value = Some(attr.unescape_and_decode_value(&xml)?),

            attr if attr.key == b"office:currency" =>
                cell_currency = Some(attr.unescape_and_decode_value(&xml)?),

            attr if attr.key == b"table:formula" =>
                cell.formula = Some(attr.unescape_and_decode_value(&xml)?),
            attr if attr.key == b"table:style-name" =>
                cell.style = Some(attr.unescape_and_decode_value(&xml)?),

            _ => {}
        }
    }

    let mut buf = Vec::new();
    loop {
        let evt = xml.read_event(&mut buf)?;
        if dump_xml { log::debug!(" style {:?}", evt); }
        match evt {
            Event::Text(xml_tag) => {
                // Not every cell type has a value attribute, some take
                // their value from the string representation.
                cell_content = text_append(cell_content, &xml_tag.unescape_and_decode(&xml)?);
            }

            Event::Start(xml_tag)
            if xml_tag.name() == b"text:p" => {
                cell_content = text_append_or(cell_content, "\n", "");
            }
            Event::Empty(xml_tag)
            if xml_tag.name() == b"text:p" => {}
            Event::End(xml_tag)
            if xml_tag.name() == b"text:p" => {}

            Event::Start(xml_tag)
            if xml_tag.name() == b"text:a" => {}
            Event::End(xml_tag)
            if xml_tag.name() == b"text:a" => {}

            Event::Empty(xml_tag)
            if xml_tag.name() == b"text:s" => {
                cell_content = text_append(cell_content, " ");
            }

            Event::End(xml_tag)
            if xml_tag.name() == tag_name => {
                cell.value = parse_value(value_type,
                                         cell_value,
                                         cell_content,
                                         cell_currency)?;

                while cell_repeat > 1 {
                    sheet.add_cell(row, col, cell.clone());
                    col += 1;
                    cell_repeat -= 1;
                }
                sheet.add_cell(row, col, cell);
                col += 1;

                break;
            }

            Event::Eof => {
                break;
            }

            _ => {}
        }

        buf.clear();
    }

    Ok(col)
}

fn text_append_or(text: Option<String>, append: &str, default: &str) -> Option<String> {
    match text {
        Some(s) => Some(s + append),
        None => Some(default.to_string())
    }
}

fn text_append(text: Option<String>, append: &str) -> Option<String> {
    match text {
        Some(s) => Some(s + append),
        None => Some(append.to_owned())
    }
}

fn read_empty_table_cell(xml: &mut quick_xml::Reader<BufReader<&mut ZipFile>>,
                         xml_tag: BytesStart,
                         row: ucell,
                         mut col: ucell,
                         sheet: &mut Sheet) -> Result<ucell, OdsError> {
    let mut cell = None;
    // Default advance is one column.
    let mut cell_repeat = 1;
    for attr in xml_tag.attributes().with_checks(false) {
        match attr? {
            attr if attr.key == b"table:number-columns-repeated" => {
                let v = attr.unescaped_value()?;
                let v = xml.decode(v.as_ref())?;
                cell_repeat = v.parse::<ucell>()?;
            }

            attr if attr.key == b"table:formula" => {
                if cell.is_none() {
                    cell = Some(SCell::new());
                }
                if let Some(c) = &mut cell {
                    c.formula = Some(attr.unescape_and_decode_value(&xml)?);
                }
            }
            attr if attr.key == b"table:style-name" => {
                if cell.is_none() {
                    cell = Some(SCell::new());
                }
                if let Some(c) = &mut cell {
                    c.style = Some(attr.unescape_and_decode_value(&xml)?);
                }
            }
            attr if attr.key == b"table:number-rows-spanned" => {
                if cell.is_none() {
                    cell = Some(SCell::new());
                }
                if let Some(c) = &mut cell {
                    let v = attr.unescape_and_decode_value(&xml)?;
                    c.span.0 = v.parse::<ucell>()?;
                }
            }
            attr if attr.key == b"table:number-columns-spanned" => {
                if cell.is_none() {
                    cell = Some(SCell::new());
                }
                if let Some(c) = &mut cell {
                    let v = attr.unescape_and_decode_value(&xml)?;
                    c.span.1 = v.parse::<ucell>()?;
                }
            }

            _ => { /* should be nothing else of interest here */ }
        }
    }

    if let Some(cell) = cell {
        while cell_repeat > 1 {
            sheet.add_cell(row, col, cell.clone());
            col += 1;
            cell_repeat -= 1;
        }
        sheet.add_cell(row, col, cell);
        col += 1;
    } else {
        col += cell_repeat;
    }

    Ok(col)
}

fn parse_value(value_type: Option<ValueType>,
               cell_value: Option<String>,
               cell_content: Option<String>,
               cell_currency: Option<String>) -> Result<Value, OdsError> {
    if let Some(value_type) = value_type {
        match value_type {
            ValueType::Empty => {
                Ok(Value::Empty)
            }
            ValueType::Text => {
                if let Some(cell_content) = cell_content {
                    Ok(Value::Text(cell_content))
                } else {
                    Ok(Value::Text("".to_string()))
                }
            }
            ValueType::Number => {
                if let Some(cell_value) = cell_value {
                    let f = cell_value.parse::<f64>()?;
                    Ok(Value::Number(f))
                } else {
                    Err(OdsError::Ods(String::from("Cell of type number, but no value!")))
                }
            }
            ValueType::DateTime => {
                if let Some(cell_value) = cell_value {
                    let dt =
                        if cell_value.len() == 10 {
                            NaiveDate::parse_from_str(cell_value.as_str(), "%Y-%m-%d")?.and_hms(0, 0, 0)
                        } else {
                            NaiveDateTime::parse_from_str(cell_value.as_str(), "%Y-%m-%dT%H:%M:%S%.f")?
                        };

                    Ok(Value::DateTime(dt))
                } else {
                    Err(OdsError::Ods(String::from("Cell of type datetime, but no value!")))
                }
            }
            ValueType::TimeDuration => {
                if let Some(mut cell_value) = cell_value {
                    let mut hour: u32 = 0;
                    let mut have_hour = false;
                    let mut min: u32 = 0;
                    let mut have_min = false;
                    let mut sec: u32 = 0;
                    let mut have_sec = false;
                    let mut nanos: u32 = 0;
                    let mut nanos_digits: u8 = 0;

                    for c in cell_value.drain(..) {
                        match c {
                            'P' | 'T' => {}
                            '0'..='9' => {
                                if !have_hour {
                                    hour = hour * 10 + (c as u32 - '0' as u32);
                                } else if !have_min {
                                    min = min * 10 + (c as u32 - '0' as u32);
                                } else if !have_sec {
                                    sec = sec * 10 + (c as u32 - '0' as u32);
                                } else {
                                    nanos = nanos * 10 + (c as u32 - '0' as u32);
                                    nanos_digits += 1;
                                }
                            }
                            'H' => have_hour = true,
                            'M' => have_min = true,
                            '.' => have_sec = true,
                            'S' => {}
                            _ => {}
                        }
                    }
                    // unseen nano digits
                    while nanos_digits < 9 {
                        nanos *= 10;
                        nanos_digits += 1;
                    }

                    let secs: u64 = hour as u64 * 3600 + min as u64 * 60 + sec as u64;
                    let dur = Duration::from_std(std::time::Duration::new(secs, nanos))?;

                    Ok(Value::TimeDuration(dur))
                } else {
                    Err(OdsError::Ods(String::from("Cell of type time-duration, but no value!")))
                }
            }
            ValueType::Boolean => {
                if let Some(cell_value) = cell_value {
                    Ok(Value::Boolean(&cell_value == "true"))
                } else {
                    Err(OdsError::Ods(String::from("Cell of type boolean, but no value!")))
                }
            }
            ValueType::Currency => {
                if let Some(cell_value) = cell_value {
                    let f = cell_value.parse::<f64>()?;
                    if let Some(cell_currency) = cell_currency {
                        Ok(Value::Currency(cell_currency, f))
                    } else {
                        Err(OdsError::Ods(String::from("Cell of type currency, but no currency name!")))
                    }
                } else {
                    Err(OdsError::Ods(String::from("Cell of type currency, but no value!")))
                }
            }
            ValueType::Percentage => {
                if let Some(cell_value) = cell_value {
                    let f = cell_value.parse::<f64>()?;
                    Ok(Value::Percentage(f))
                } else {
                    Err(OdsError::Ods(String::from("Cell of type percentage, but no value!")))
                }
            }
        }
    } else {
        Err(OdsError::Ods(String::from("Cell with no value-type!")))
    }
}

fn decode_value_type(attr: Attribute) -> Result<ValueType, OdsError> {
    match attr.unescaped_value()?.as_ref() {
        b"string" => Ok(ValueType::Text),
        b"float" => Ok(ValueType::Number),
        b"percentage" => Ok(ValueType::Percentage),
        b"date" => Ok(ValueType::DateTime),
        b"time" => Ok(ValueType::TimeDuration),
        b"boolean" => Ok(ValueType::Boolean),
        b"currency" => Ok(ValueType::Currency),
        other => Err(OdsError::Ods(format!("Unknown cell-type {:?}", other)))
    }
}

#[allow(clippy::single_match)]
fn read_fonts(book: &mut WorkBook,
              xml: &mut quick_xml::Reader<BufReader<&mut ZipFile>>,
              end_tag: &[u8],
              dump_xml: bool) -> Result<(), OdsError> {
    let mut buf = Vec::new();

    let mut font: FontDecl = FontDecl::new_origin(XMLOrigin::Content);

    loop {
        let evt = xml.read_event(&mut buf)?;
        if dump_xml { log::debug!(" style {:?}", evt); }
        match evt {
            Event::Start(ref xml_tag)
            | Event::Empty(ref xml_tag) => {
                match xml_tag.name() {
                    b"style:font-face" => {
                        for attr in xml_tag.attributes().with_checks(false) {
                            match attr? {
                                attr if attr.key == b"style:name" => {
                                    let v = attr.unescape_and_decode_value(&xml)?;
                                    font.set_name(v);
                                }
                                attr => {
                                    let k = xml.decode(&attr.key)?;
                                    let v = attr.unescape_and_decode_value(&xml)?;
                                    font.set_prp(k, v);
                                }
                            }
                        }

                        book.add_font(font);
                        font = FontDecl::new_origin(XMLOrigin::Content);
                    }
                    _ => {}
                }
            }

            Event::End(ref e) => {
                if e.name() == end_tag {
                    break;
                }
            }

            Event::Eof => {
                break;
            }
            _ => {}
        }

        buf.clear();
    }

    Ok(())
}

fn read_styles(book: &mut WorkBook,
               xml: &mut quick_xml::Reader<BufReader<&mut ZipFile>>,
               end_tag: &[u8],
               dump_xml: bool) -> Result<(), OdsError> {
    let mut buf = Vec::new();

    let mut style: Style = Style::new_origin(XMLOrigin::Content);
    let mut value_style = ValueFormat::new_origin(XMLOrigin::Content);
    // Styles with content information are stored before completion.
    let mut value_style_part = None;

    loop {
        let evt = xml.read_event(&mut buf)?;
        if dump_xml { log::debug!(" style {:?}", evt); }
        match evt {
            Event::Start(ref xml_tag)
            | Event::Empty(ref xml_tag) => {
                match xml_tag.name() {
                    b"style:style" => {
                        read_style(xml, xml_tag, &mut style)?;

                        // In case of an empty xml-tag we are done here.
                        if let Event::Empty(_) = evt {
                            book.add_style(style);
                            style = Style::new_origin(XMLOrigin::Content);
                        }
                    }

                    b"style:table-properties" =>
                        copy_style_properties(&mut style, &Style::set_table_prp, xml, xml_tag)?,
                    b"style:table-column-properties" =>
                        copy_style_properties(&mut style, &Style::set_table_col_prp, xml, xml_tag)?,
                    b"style:table-row-properties" =>
                        copy_style_properties(&mut style, &Style::set_table_row_prp, xml, xml_tag)?,
                    b"style:table-cell-properties" =>
                        copy_style_properties(&mut style, &Style::set_table_cell_prp, xml, xml_tag)?,
                    b"style:text-properties" =>
                        copy_style_properties(&mut style, &Style::set_text_prp, xml, xml_tag)?,
                    b"style:paragraph-properties" =>
                        copy_style_properties(&mut style, &Style::set_paragraph_prp, xml, xml_tag)?,

                    b"number:boolean-style" =>
                        read_value_format(ValueType::Boolean, &mut value_style, xml, xml_tag)?,
                    b"number:date-style" =>
                        read_value_format(ValueType::DateTime, &mut value_style, xml, xml_tag)?,
                    b"number:time-style" =>
                        read_value_format(ValueType::TimeDuration, &mut value_style, xml, xml_tag)?,
                    b"number:number-style" =>
                        read_value_format(ValueType::Number, &mut value_style, xml, xml_tag)?,
                    b"number:currency-style" =>
                        read_value_format(ValueType::Currency, &mut value_style, xml, xml_tag)?,
                    b"number:percentage-style" =>
                        read_value_format(ValueType::Percentage, &mut value_style, xml, xml_tag)?,
                    b"number:text-style" =>
                        read_value_format(ValueType::Text, &mut value_style, xml, xml_tag)?,

                    b"number:boolean" =>
                        push_value_format_part(&mut value_style, FormatPartType::Boolean, xml, xml_tag)?,
                    b"number:number" =>
                        push_value_format_part(&mut value_style, FormatPartType::Number, xml, xml_tag)?,
                    b"number:scientific-number" =>
                        push_value_format_part(&mut value_style, FormatPartType::Scientific, xml, xml_tag)?,
                    b"number:day" =>
                        push_value_format_part(&mut value_style, FormatPartType::Day, xml, xml_tag)?,
                    b"number:month" =>
                        push_value_format_part(&mut value_style, FormatPartType::Month, xml, xml_tag)?,
                    b"number:year" =>
                        push_value_format_part(&mut value_style, FormatPartType::Year, xml, xml_tag)?,
                    b"number:era" =>
                        push_value_format_part(&mut value_style, FormatPartType::Era, xml, xml_tag)?,
                    b"number:day-of-week" =>
                        push_value_format_part(&mut value_style, FormatPartType::DayOfWeek, xml, xml_tag)?,
                    b"number:week-of-year" =>
                        push_value_format_part(&mut value_style, FormatPartType::WeekOfYear, xml, xml_tag)?,
                    b"number:quarter" =>
                        push_value_format_part(&mut value_style, FormatPartType::Quarter, xml, xml_tag)?,
                    b"number:hours" =>
                        push_value_format_part(&mut value_style, FormatPartType::Hours, xml, xml_tag)?,
                    b"number:minutes" =>
                        push_value_format_part(&mut value_style, FormatPartType::Minutes, xml, xml_tag)?,
                    b"number:seconds" =>
                        push_value_format_part(&mut value_style, FormatPartType::Seconds, xml, xml_tag)?,
                    b"number:fraction" =>
                        push_value_format_part(&mut value_style, FormatPartType::Fraction, xml, xml_tag)?,
                    b"number:am-pm" =>
                        push_value_format_part(&mut value_style, FormatPartType::AmPm, xml, xml_tag)?,
                    b"number:embedded-text" =>
                        push_value_format_part(&mut value_style, FormatPartType::EmbeddedText, xml, xml_tag)?,
                    b"number:text-content" =>
                        push_value_format_part(&mut value_style, FormatPartType::TextContent, xml, xml_tag)?,
                    b"style:text" =>
                        push_value_format_part(&mut value_style, FormatPartType::Day, xml, xml_tag)?,
                    b"style:map" =>
                        push_value_format_part(&mut value_style, FormatPartType::StyleMap, xml, xml_tag)?,
                    b"number:currency-symbol" => {
                        value_style_part = Some(read_part(xml, xml_tag, FormatPartType::CurrencySymbol)?);

                        // Empty-Tag. Finish here.
                        if let Event::Empty(_) = evt {
                            if let Some(part) = value_style_part {
                                value_style.push_part(part);
                            }
                            value_style_part = None;
                        }
                    }
                    b"number:text" => {
                        value_style_part = Some(read_part(xml, xml_tag, FormatPartType::Text)?);

                        // Empty-Tag. Finish here.
                        if let Event::Empty(_) = evt {
                            if let Some(part) = value_style_part {
                                value_style.push_part(part);
                            }
                            value_style_part = None;
                        }
                    }

                    _ => {}
                }
            }

            Event::Text(ref e) => {
                if let Some(part) = &mut value_style_part {
                    part.content = Some(e.unescape_and_decode(&xml)?);
                }
            }

            Event::End(ref e) => {
                if e.name() == end_tag {
                    break;
                }

                match e.name() {
                    b"style:style" => {
                        book.add_style(style);
                        style = Style::new_origin(XMLOrigin::Content);
                    }
                    b"number:boolean-style" |
                    b"number:date-style" |
                    b"number:time-style" |
                    b"number:number-style" |
                    b"number:currency-style" |
                    b"number:percentage-style" |
                    b"number:text-style" => {
                        book.add_format(value_style);
                        value_style = ValueFormat::new_origin(XMLOrigin::Content);
                    }
                    b"number:currency-symbol" | b"number:text" => {
                        if let Some(part) = value_style_part {
                            value_style.push_part(part);
                        }
                        value_style_part = None;
                    }

                    _ => {}
                }
            }
            Event::Eof => {
                break;
            }
            _ => {}
        }

        buf.clear();
    }

    Ok(())
}

fn read_value_format(value_type: ValueType,
                     value_style: &mut ValueFormat,
                     xml: &mut quick_xml::Reader<BufReader<&mut ZipFile>>,
                     xml_tag: &BytesStart) -> Result<(), OdsError> {
    value_style.v_type = value_type;

    for attr in xml_tag.attributes().with_checks(false) {
        match attr? {
            attr if attr.key == b"style:name" => {
                let v = attr.unescape_and_decode_value(&xml)?;
                value_style.set_name(v);
            }
            attr => {
                let k = xml.decode(&attr.key)?;
                let v = attr.unescape_and_decode_value(&xml)?;
                value_style.set_prp(k, v);
            }
        }
    }

    Ok(())
}

fn push_value_format_part(value_style: &mut ValueFormat,
                          part_type: FormatPartType,
                          xml: &mut quick_xml::Reader<BufReader<&mut ZipFile>>,
                          xml_tag: &BytesStart) -> Result<(), OdsError> {
    value_style.push_part(read_part(xml, xml_tag, part_type)?);

    Ok(())
}

fn read_part(xml: &mut quick_xml::Reader<BufReader<&mut ZipFile>>,
             xml_tag: &BytesStart,
             part_type: FormatPartType) -> Result<FormatPart, OdsError> {
    let mut part = FormatPart::new(part_type);

    for a in xml_tag.attributes().with_checks(false) {
        if let Ok(attr) = a {
            let k = xml.decode(&attr.key)?;
            let v = attr.unescape_and_decode_value(&xml)?;

            part.set_prp(k, v);
        }
    }

    Ok(part)
}

fn read_style(xml: &mut quick_xml::Reader<BufReader<&mut ZipFile>>,
              xml_tag: &BytesStart,
              style: &mut Style) -> Result<(), OdsError> {
    for attr in xml_tag.attributes().with_checks(false) {
        match attr? {
            attr if attr.key == b"style:name" => {
                let v = attr.unescape_and_decode_value(&xml)?;
                style.set_name(v);
            }
            attr if attr.key == b"style:family" => {
                let v = attr.unescape_and_decode_value(&xml)?;
                match v.as_ref() {
                    "table" => style.family = StyleFor::Table,
                    "table-column" => style.family = StyleFor::TableColumn,
                    "table-row" => style.family = StyleFor::TableRow,
                    "table-cell" => style.family = StyleFor::TableCell,
                    _ => {}
                }
            }
            attr if attr.key == b"style:parent-style-name" => {
                let v = attr.unescape_and_decode_value(&xml)?;
                style.parent = Some(v);
            }
            attr if attr.key == b"style:data-style-name" => {
                let v = attr.unescape_and_decode_value(&xml)?;
                style.value_format = Some(v);
            }
            _ => { /* noop */ }
        }
    }

    Ok(())
}

fn copy_style_properties(style: &mut Style,
                         add_fn: &dyn Fn(&mut Style, &str, String),
                         xml: &mut quick_xml::Reader<BufReader<&mut ZipFile>>,
                         xml_tag: &BytesStart) -> Result<(), OdsError> {
    for attr in xml_tag.attributes().with_checks(false) {
        if let Ok(attr) = attr {
            let k = xml.decode(&attr.key)?;
            let v = attr.unescape_and_decode_value(&xml)?;
            add_fn(style, k, v);
        }
    }

    Ok(())
}