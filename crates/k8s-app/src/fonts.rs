//! The product's bundled typeface.
//!
//! Inter, one variable file, compiled into the binary.
//!
//! # Why the app ships a font at all
//!
//! The three platforms this runs on disagree about letterforms. SF, Segoe UI and
//! Cantarell have different advance widths, different x-heights and different
//! default line heights, so the same column of pod names is three different
//! widths on three different machines — and a table that has to truncate a name
//! truncates a different name on each of them. This product spends most of its
//! life in a table, so metric consistency outranks platform consistency: one
//! file, one set of advances, the same pixels everywhere.
//!
//! Settings still offer "follow the system", which is the honest path for a
//! reader who wants their platform's look. It is an option rather than the
//! default because the default has to be the one that does not surprise.
//!
//! # Why the whole variable file
//!
//! The type scale uses exactly three weights — 400, 500 and 600 — and a variable
//! font carries all of them in one file. Shipping three static cuts would be
//! three files to keep in step with the scale for no rendering benefit.

use gpui_kit::{App, Font, font};

/// The family name the bundled file registers under, and the name every theme
/// asks for.
///
/// # This string is a property of the file, not of the product
///
/// A text system resolves a family by an **exact** string match against the
/// family names the font data declares, and it does so on all three platforms
/// the same way: Linux matches `name` ID 1 (via `fontdb`), Windows asks
/// DirectWrite for the face's family names (ID 1, or ID 16 when the file has
/// one), and macOS asks CoreText for the same. There is no alias table, no
/// substring match and no "did you mean".
///
/// So the name the app *asks* for has to be the name `InterVariable.ttf`
/// *declares*. That file says `Inter Variable` — the variable release of the
/// family names the whole variable file, not a style of a static one — and
/// asking for `Inter` is not a near miss that a text system forgives: it is a
/// miss. `add_fonts` still succeeds, no error is logged, and every `font(…)`
/// in the app lands on the platform's default face.
///
/// The symptom is the whole UI in the platform's monospace fallback, which is
/// exactly what a column-width bug looks like and nothing like a font bug, so
/// `the_bundled_file_registers_under_the_name_the_code_asks_for` reads the
/// family back out of the compiled-in bytes and fails if the two ever drift.
pub const INTER_FAMILY: &str = "Inter Variable";

/// Inter Variable, all weights, OFL 1.1. `Inter-LICENSE` ships beside it.
const INTER_VARIABLE: &[u8] = include_bytes!("../assets/fonts/InterVariable.ttf");

/// Registers the bundled typeface with the running text system.
///
/// Called once at startup, before any window opens and before any font is
/// requested. A font that is asked for before it is registered falls back to the
/// platform font silently, and a silent fallback is precisely the thing this
/// module exists to prevent — the layout would look right and the metrics would
/// be the platform's.
pub fn install(cx: &mut App) {
    if let Err(error) = cx
        .text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(INTER_VARIABLE)])
    {
        // A missing typeface is a legibility problem, not a reason to refuse to
        // start: the app falls back to the platform font and everything else
        // still works. It does have to be said out loud, because the symptom
        // otherwise shows up much later as mis-measured columns.
        eprintln!(
            "k8s-gpui: Inter Variable did not register, so text falls back to the platform \
             font and table columns will measure differently on each OS: {error}"
        );
        return;
    }
    // `add_fonts` succeeding only means the bytes parsed. Whether the family the
    // app asks for is *resolvable* is a separate question the return value does
    // not answer, and a miss there is silent, so it is asked directly. A name
    // the text system cannot resolve is the failure this module exists to
    // prevent, and it is worth saying out loud at startup instead of letting it
    // surface much later as a table that measures differently per machine.
    if !cx
        .text_system()
        .all_font_names()
        .iter()
        .any(|name| name == INTER_FAMILY)
    {
        eprintln!(
            "k8s-gpui: the bundled typeface registered, but no family named {INTER_FAMILY:?} is \
             resolvable, so every font(\"{INTER_FAMILY}\") in the app still falls back to the \
             platform face and table columns will measure differently on each OS"
        );
    }
}

/// The product's UI typeface.
///
/// Every piece of chrome, every label, every table cell and every button reads
/// this. The only text that does not is the monospace data line — YAML, logs,
/// UIDs, IPs, ports and image tags — which is the one place where a value is
/// shaped rather than spelled and alignment is the point.
pub fn ui_font() -> Font {
    font(INTER_FAMILY)
}
#[cfg(test)]
mod tests {
    use super::{INTER_FAMILY, INTER_VARIABLE};

    /// The family name a text system will resolve `INTER_VARIABLE` under, read
    /// back out of the file's own `name` table.
    ///
    /// This is the check the app never gets for free. `add_fonts` reports
    /// whether the bytes parsed, not whether the family they declare is the
    /// family the app asks for, and a mismatch between the two is invisible at
    /// startup and unmistakable on screen: the whole interface in the
    /// platform's default face, which reads as a layout bug rather than a font
    /// one.
    fn declared_family(bytes: &[u8]) -> Option<String> {
        /// `u16` big-endian, which is how the sfnt directory and the `name`
        /// table's own header are both written.
        fn be16(bytes: &[u8], at: usize) -> Option<u16> {
            Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
        }
        /// `u32` big-endian.
        fn be32(bytes: &[u8], at: usize) -> Option<u32> {
            Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
        }

        if bytes.get(..4)? != [0x00, 0x01, 0x00, 0x00] {
            return None;
        }
        let table_count = be16(bytes, 4)? as usize;
        // Each directory record is a 4-byte tag, a checksum, an offset and a
        // length, so the `name` table is found by tag rather than by position:
        // the record order is not fixed by the format.
        let name_table = (0..table_count).find_map(|index| {
            let record = 12 + index * 16;
            if bytes.get(record..record + 4)? != b"name" {
                return None;
            }
            Some((
                be32(bytes, record + 8)? as usize,
                be32(bytes, record + 12)? as usize,
            ))
        })?;

        let (base, length) = name_table;
        let table = bytes.get(base..base + length)?;
        // `name` is `format u16, count u16, stringOffset u16`, then one
        // 12-byte record per name, then the string pool.
        let count = be16(table, 2)? as usize;
        let pool = be16(table, 4)? as usize;
        (0..count).find_map(|index| {
            let record = 6 + index * 12;
            // Name ID 1 is the family name. ID 6 is the PostScript name
            // ("InterVariable") and is deliberately *not* accepted here: it is
            // what a text system matches faces by, not what a user selects a
            // family by, and asking for it would be the same class of miss.
            if be16(table, record + 6)? != 1 {
                return None;
            }
            let start = pool.checked_add(be16(table, record + 10)? as usize)?;
            let end = start.checked_add(be16(table, record + 8)? as usize)?;
            // Platform 3 (Windows) and platform 0 (Unicode) store the string as
            // UTF-16BE; platform 1 (Macintosh) stores it as single bytes. The
            // bundled file carries both encodings of the same ASCII name.
            Some(match be16(table, record)? {
                1 => std::str::from_utf8(table.get(start..end)?).ok()?.to_owned(),
                _ => {
                    let units: Vec<u16> = table
                        .get(start..end)?
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                        .collect();
                    String::from_utf16(&units).ok()?
                }
            })
        })
    }

    /// The name the app asks for has to be the name the file declares.
    ///
    /// Everything above this one line is why: the two strings have to be equal
    /// and neither module can be trusted to keep them equal on its own, so
    /// this reads the answer out of the compiled-in bytes instead of out of
    /// either module.
    #[test]
    fn the_bundled_file_registers_under_the_name_the_code_asks_for() {
        assert_eq!(
            declared_family(INTER_VARIABLE).as_deref(),
            Some(INTER_FAMILY),
            "the bundled typeface declares a different family than the app asks for, so every \
             font(\"{INTER_FAMILY}\") silently falls back to the platform face"
        );
    }
}
