//! The bundled UI font: Inter 4.1 (SIL Open Font License, see
//! `assets/fonts/LICENSE.txt`), as static faces.
//!
//! Static files, not the variable font: gpui draws a variable font at its
//! default instance, and a system UI font that is missing (gpui's Linux
//! default is IBM Plex Sans) falls back at normal weight, so bold text never
//! looked bold. One face per weight the UI uses (normal, medium, semibold,
//! bold) plus italic and bold italic for chat markdown. About 2.5 MB.

use std::borrow::Cow;

use gpui_kit::App;

/// The family name the theme's `font.family` names.
pub const UI_FAMILY: &str = "Inter";

const FACES: [&[u8]; 6] = [
    include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../assets/fonts/Inter-Italic.ttf"),
    include_bytes!("../assets/fonts/Inter-Medium.ttf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Inter-Bold.ttf"),
    include_bytes!("../assets/fonts/Inter-BoldItalic.ttf"),
];

/// Adds the faces to the text system. Call before the first window opens.
pub fn register(cx: &mut App) {
    let faces = FACES.iter().map(|f| Cow::Borrowed(*f)).collect();
    if let Err(e) = cx.text_system().add_fonts(faces) {
        tracing::warn!("couldn't load the bundled {UI_FAMILY} font: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::{FACES, UI_FAMILY};

    fn u16_at(b: &[u8], at: usize) -> usize {
        u16::from_be_bytes([b[at], b[at + 1]]) as usize
    }

    fn u32_at(b: &[u8], at: usize) -> usize {
        u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
    }

    /// A table's bytes from an sfnt font file.
    fn table<'a>(font: &'a [u8], tag: &[u8; 4]) -> &'a [u8] {
        let tables = u16_at(font, 4);
        (0..tables)
            .map(|i| 12 + 16 * i)
            .find(|&rec| &font[rec..rec + 4] == tag)
            .map(|rec| {
                let (offset, len) = (u32_at(font, rec + 8), u32_at(font, rec + 12));
                &font[offset..offset + len]
            })
            .expect("table present")
    }

    /// The family name text systems group by: the typographic family (name
    /// id 16) when present, else the family (1). Windows/Unicode strings.
    fn family(font: &[u8]) -> String {
        let name = table(font, b"name");
        let (count, strings) = (u16_at(name, 2), u16_at(name, 4));
        let get = |id: usize| {
            (0..count).map(|i| 6 + 12 * i).find_map(|rec| {
                let platform = u16_at(name, rec);
                if platform != 3 || u16_at(name, rec + 6) != id {
                    return None;
                }
                let (len, off) = (u16_at(name, rec + 8), u16_at(name, rec + 10));
                let raw = &name[strings + off..strings + off + len];
                let units: Vec<u16> = raw
                    .chunks(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16(&units).ok()
            })
        };
        get(16).or_else(|| get(1)).expect("family name")
    }

    #[test]
    fn faces_are_one_family_with_the_weights_the_ui_uses() {
        let mut faces: Vec<(usize, bool)> = FACES
            .iter()
            .map(|font| {
                assert_eq!(family(font), UI_FAMILY);
                let os2 = table(font, b"OS/2");
                // usWeightClass, and fsSelection bit 0 (italic).
                (u16_at(os2, 4), u16_at(os2, 62) & 1 == 1)
            })
            .collect();
        faces.sort();
        assert_eq!(
            faces,
            vec![
                (400, false),
                (400, true),
                (500, false),
                (600, false),
                (700, false),
                (700, true)
            ]
        );
    }
}
