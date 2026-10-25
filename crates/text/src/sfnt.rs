//! Minimal SFNT (TrueType / OpenType / collection) header reader for font discovery: reads only
//! the table directory, `name` and `OS/2` of each face, so scanning a system font folder does not
//! load whole files.

use std::io::{Read, Seek, SeekFrom};

/// Names and style attributes of one face in a font file.
#[derive(Clone, Debug, PartialEq)]
pub struct FaceNames {
    pub index: u32,
    pub family: String,
    pub style: String,
    pub full_name: String,
    pub weight: u16,
    pub italic: bool,
    /// The family name in another language than English (CJK, Arabic… fonts), when the font
    /// has one (Settings ▸ Type ▸ Show Font Names in English off shows it).
    pub native_family: Option<String>,
    /// The PostScript name (name id 6, e.g. `YuGothic-Bold`): what After Effects scripts and
    /// `.aep`-derived data call a font.
    pub postscript: Option<String>,
    /// A variable font: it has an `fvar` table with at least one axis (the font menu's
    /// Variable Fonts filter).
    pub variable: bool,
    /// A variable font's named instances (`fvar`): the styles it offers beyond its default
    /// (SF Pro's Thin … Black), each with its axis coordinates.
    pub instances: Vec<NamedInstance>,
}

/// One named instance of a variable font (`fvar` InstanceRecord): a style name such as `Bold`
/// and the design-space position it stands for.
#[derive(Clone, Debug, PartialEq)]
pub struct NamedInstance {
    /// The subfamily name (`Bold`, `Condensed Light`, `Bold Italic`).
    pub name: String,
    /// The instance's PostScript name (`SFPro-Bold`), when the font gives one.
    pub postscript: Option<String>,
    /// Axis values in user units (tag as in the font, e.g. `wght`), one per axis.
    pub coords: Vec<(String, f32)>,
}

fn be16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?]))
}
fn be32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?, *b.get(o + 3)?]))
}

fn read_at<R: Read + Seek>(r: &mut R, off: u64, len: usize) -> Option<Vec<u8>> {
    if len > 4 << 20 {
        return None;
    }
    r.seek(SeekFrom::Start(off)).ok()?;
    let mut v = vec![0u8; len];
    r.read_exact(&mut v).ok()?;
    Some(v)
}

/// Read the names of every face in a font file (a single font or a collection).
pub fn read_faces<R: Read + Seek>(r: &mut R) -> Vec<FaceNames> {
    let Some(head) = read_at(r, 0, 12) else { return Vec::new() };
    let offsets: Vec<u32> = if &head[0..4] == b"ttcf" {
        let n = be32(&head, 8).unwrap_or(0).min(256) as usize;
        let Some(t) = read_at(r, 12, n * 4) else { return Vec::new() };
        (0..n).filter_map(|i| be32(&t, i * 4)).collect()
    } else {
        vec![0]
    };
    offsets.iter().enumerate().filter_map(|(i, &o)| read_face(r, o as u64, i as u32)).collect()
}

/// Names from in-memory font data.
pub fn read_faces_bytes(data: &[u8]) -> Vec<FaceNames> {
    read_faces(&mut std::io::Cursor::new(data))
}

fn read_face<R: Read + Seek>(r: &mut R, off: u64, index: u32) -> Option<FaceNames> {
    let h = read_at(r, off, 12)?;
    let tag = &h[0..4];
    if tag != [0, 1, 0, 0] && tag != b"OTTO" && tag != b"true" {
        return None;
    }
    let n = be16(&h, 4)? as usize;
    let dir = read_at(r, off + 12, n * 16)?;
    let mut name = None;
    let mut os2 = None;
    let mut fvar = None;
    for i in 0..n {
        let rec = &dir[i * 16..i * 16 + 16];
        let (o, l) = (be32(rec, 8)? as u64, be32(rec, 12)? as usize);
        match &rec[0..4] {
            b"name" => name = Some((o, l)),
            b"OS/2" => os2 = Some((o, l)),
            b"fvar" => fvar = Some((o, l)),
            _ => {}
        }
    }
    let (no, nl) = name?;
    let nb = read_at(r, no, nl)?;
    let get = |id: u16| name_string(&nb, id);
    let family = get(16).or_else(|| get(1))?;
    let style = get(17).or_else(|| get(2)).unwrap_or_else(|| "Regular".into());
    let full_name = get(4).unwrap_or_else(|| format!("{family} {style}"));
    let native_family = native_name(&nb, 16).or_else(|| native_name(&nb, 1)).filter(|n| *n != family);
    let postscript = get(6).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let (mut weight, mut italic) = (400u16, false);
    if let Some((oo, ol)) = os2
        && let Some(ob) = read_at(r, oo, ol.min(78))
    {
        weight = be16(&ob, 4).unwrap_or(400);
        italic = be16(&ob, 62).is_some_and(|fs| fs & 1 != 0);
    }
    let sl = style.to_ascii_lowercase();
    italic |= sl.contains("italic") || sl.contains("oblique");
    // fvar header: version (4), axesArrayOffset (2), reserved (2), axisCount (2).
    // The whole table (named instances follow the axes); just the header when the table is too
    // large or runs past the end of the file.
    let fvar_bytes = fvar.filter(|(_, l)| *l >= 10).and_then(|(fo, fl)| read_at(r, fo, fl).or_else(|| read_at(r, fo, 10)));
    let variable = fvar_bytes.as_deref().and_then(|fb| be16(fb, 8)).is_some_and(|axes| axes > 0);
    let instances = if variable { fvar_bytes.as_deref().map(|fb| read_instances(fb, &nb)).unwrap_or_default() } else { Vec::new() };
    Some(FaceNames { index, family, style, full_name, weight, italic, native_family, postscript, variable, instances })
}

/// The named instances of an `fvar` table (`fb`), named through the face's `name` table (`nb`).
/// Every offset comes from the file, so all arithmetic is checked and the count is capped.
fn read_instances(fb: &[u8], nb: &[u8]) -> Vec<NamedInstance> {
    let field = |o: usize| be16(fb, o).map(usize::from);
    let (Some(axes_off), Some(axis_count), Some(axis_size), Some(inst_count), Some(inst_size)) = (field(4), field(8), field(10), field(12), field(14)) else {
        return Vec::new();
    };
    // An instance record holds subfamilyNameID, flags and one Fixed per axis (then an optional
    // postScriptNameID).
    let Some(coords_len) = axis_count.checked_mul(4) else { return Vec::new() };
    if axis_count == 0 || axis_size < 20 || inst_size < coords_len.saturating_add(4) {
        return Vec::new();
    }
    let mut tags = Vec::with_capacity(axis_count.min(64));
    for i in 0..axis_count {
        let Some(t) = i.checked_mul(axis_size).and_then(|o| o.checked_add(axes_off)).and_then(|o| fb.get(o..o.checked_add(4)?)) else {
            return Vec::new();
        };
        tags.push(String::from_utf8_lossy(t).to_string());
    }
    let Some(start) = axis_count.checked_mul(axis_size).and_then(|n| n.checked_add(axes_off)) else { return Vec::new() };
    let has_ps = inst_size >= coords_len.saturating_add(6);
    let mut out = Vec::new();
    for i in 0..inst_count.min(1024) {
        let Some(o) = i.checked_mul(inst_size).and_then(|n| n.checked_add(start)) else { break };
        let Some(name_id) = be16(fb, o) else { break };
        let mut coords = Vec::with_capacity(tags.len());
        for (k, t) in tags.iter().enumerate() {
            let Some(v) = o.checked_add(4 + 4 * k).and_then(|p| be32(fb, p)) else { break };
            coords.push((t.clone(), v as i32 as f32 / 65536.0));
        }
        if coords.len() != tags.len() {
            break;
        }
        let Some(name) = name_string(nb, name_id).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else { continue };
        let postscript = if has_ps { o.checked_add(4 + coords_len).and_then(|p| be16(fb, p)) } else { None }
            .filter(|&id| id != 0xFFFF)
            .and_then(|id| name_string(nb, id))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if !out.iter().any(|n: &NamedInstance| n.name.eq_ignore_ascii_case(&name)) {
            out.push(NamedInstance { name, postscript, coords });
        }
    }
    out
}

/// A Windows Unicode name-table string by id in a language other than US English.
fn native_name(b: &[u8], id: u16) -> Option<String> {
    let count = be16(b, 2)? as usize;
    let storage = be16(b, 4)? as usize;
    for i in 0..count {
        let r = 6 + i * 12;
        let (plat, enc, lang, nid, len, off) =
            (be16(b, r)?, be16(b, r + 2)?, be16(b, r + 4)?, be16(b, r + 6)?, be16(b, r + 8)? as usize, be16(b, r + 10)? as usize);
        if nid != id || plat != 3 || !(enc == 1 || enc == 10) || lang == 0x409 {
            continue;
        }
        let raw = b.get(storage + off..storage + off + len)?;
        let u: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        let s = String::from_utf16_lossy(&u);
        if !s.trim().is_empty() {
            return Some(s);
        }
    }
    None
}

/// A name-table string by id, preferring Windows Unicode English, then any Unicode, then Mac Roman.
fn name_string(b: &[u8], id: u16) -> Option<String> {
    let count = be16(b, 2)? as usize;
    let storage = be16(b, 4)? as usize;
    let mut best: Option<(u8, String)> = None;
    for i in 0..count {
        let r = 6 + i * 12;
        let (plat, enc, lang, nid, len, off) =
            (be16(b, r)?, be16(b, r + 2)?, be16(b, r + 4)?, be16(b, r + 6)?, be16(b, r + 8)? as usize, be16(b, r + 10)? as usize);
        if nid != id {
            continue;
        }
        let raw = b.get(storage + off..storage + off + len)?;
        let (rank, s) = match plat {
            3 if enc == 1 || enc == 10 => {
                let u: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                (if lang == 0x409 { 0 } else { 1 }, String::from_utf16_lossy(&u))
            }
            0 => {
                let u: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                (2, String::from_utf16_lossy(&u))
            }
            1 if enc == 0 => (3, raw.iter().map(|&c| if c < 128 { c as char } else { '?' }).collect()),
            _ => continue,
        };
        if best.as_ref().is_none_or(|(r0, _)| rank < *r0) {
            best = Some((rank, s));
        }
    }
    best.map(|(_, s)| s).filter(|s| !s.trim().is_empty())
}

/// A font of just `name` and `fvar` (one `wght` axis 100–900, default 400) with named instances
/// Regular (400), Bold (700, PostScript `<ps>-Bold`) and Light (300, no PostScript name), for
/// tests of named-instance styles that don't depend on what is installed.
#[cfg(test)]
pub(crate) fn test_instances_font(family: &str, ps: &str) -> Vec<u8> {
    let u16s = |v: &[u16]| -> Vec<u8> { v.iter().flat_map(|x| x.to_be_bytes()).collect() };
    let names: Vec<(u16, String)> =
        vec![(1, family.into()), (2, "Regular".into()), (6, format!("{ps}-Regular")), (256, "Bold".into()), (257, format!("{ps}-Bold")), (258, "Light".into())];
    let strings: Vec<Vec<u8>> = names.iter().map(|(_, s)| s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()).collect();
    let mut name = u16s(&[0, names.len() as u16, 6 + 12 * names.len() as u16]);
    let mut off = 0;
    for ((id, _), st) in names.iter().zip(&strings) {
        name.extend(u16s(&[3, 1, 0x409, *id, st.len() as u16, off]));
        off += st.len() as u16;
    }
    strings.iter().for_each(|st| name.extend(st));
    // Header: version 1.0, axes at 16, 1 axis of 20 bytes, 3 instances of 10 bytes.
    let mut fvar = u16s(&[1, 0, 16, 2, 1, 20, 3, 10]);
    fvar.extend(b"wght".iter().copied().chain(u16s(&[100, 0, 400, 0, 900, 0, 0, 256])));
    for (name_id, wght, ps_id) in [(2u16, 400u16, 6u16), (256, 700, 257), (258, 300, 0xFFFF)] {
        fvar.extend(u16s(&[name_id, 0, wght, 0, ps_id]));
    }
    let tables: [(&[u8; 4], Vec<u8>); 2] = [(b"fvar", fvar), (b"name", name)];
    let mut out = u16s(&[1, 0, tables.len() as u16, 0, 0, 0]);
    let mut body: Vec<u8> = vec![];
    for (tag, data) in &tables {
        out.extend(*tag);
        out.extend([0u8; 4]);
        out.extend(((12 + 16 * tables.len() + body.len()) as u32).to_be_bytes());
        out.extend((data.len() as u32).to_be_bytes());
        body.extend(data);
    }
    out.extend(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A variable font's named instances come from `fvar`, named through `name`; a damaged
    /// instance array yields what can be read and never panics.
    #[test]
    fn reads_named_instances_from_fvar() {
        let font = test_instances_font("Instance Test", "InstanceTest");
        let f = read_faces_bytes(&font);
        assert_eq!(f.len(), 1);
        assert!(f[0].variable);
        let names: Vec<&str> = f[0].instances.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["Regular", "Bold", "Light"]);
        let bold = &f[0].instances[1];
        assert_eq!(bold.coords, vec![("wght".to_string(), 700.0)]);
        assert_eq!(bold.postscript.as_deref(), Some("InstanceTest-Bold"));
        assert_eq!(f[0].instances[2].postscript, None, "0xFFFF means no PostScript name");
        // Every truncation of the fvar table reads without panicking.
        let fvar_at = font.windows(4).position(|w| w == b"fvar").unwrap();
        let off = u32::from_be_bytes(font[fvar_at + 8..fvar_at + 12].try_into().unwrap()) as usize;
        let len = u32::from_be_bytes(font[fvar_at + 12..fvar_at + 16].try_into().unwrap()) as usize;
        let name_at = font.windows(4).position(|w| w == b"name").unwrap();
        let noff = u32::from_be_bytes(font[name_at + 8..name_at + 12].try_into().unwrap()) as usize;
        let nlen = u32::from_be_bytes(font[name_at + 12..name_at + 16].try_into().unwrap()) as usize;
        let nb = &font[noff..noff + nlen];
        for cut in 0..len {
            let _ = read_instances(&font[off..off + cut], nb);
        }
        // Hostile counts and sizes.
        let mut bad = font[off..off + len].to_vec();
        bad[12..14].copy_from_slice(&u16::MAX.to_be_bytes());
        bad[14..16].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(read_instances(&bad, nb).len() <= 3);
        bad[8..10].copy_from_slice(&u16::MAX.to_be_bytes());
        bad[10..12].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(read_instances(&bad, nb).is_empty());
    }

    #[test]
    fn reads_bundled_names() {
        let f = read_faces_bytes(crate::fonts::INTER_BOLD);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].family, "Inter");
        assert_eq!(f[0].style, "Bold");
        assert_eq!(f[0].weight, 700);
        assert!(!f[0].italic);
        let i = read_faces_bytes(crate::fonts::INTER_ITALIC);
        assert!(i[0].italic, "{:?}", i[0]);
        let n = read_faces_bytes(crate::fonts::NOTO_SERIF_REGULAR);
        assert_eq!(n[0].family, "Noto Serif");
        assert!(read_faces_bytes(b"not a font").is_empty());
        assert!(!f[0].variable && !n[0].variable, "the bundled fonts are static");
    }

    /// A font file of just `name` (family, Regular) and, when `fvar` is given, an `fvar` table
    /// with that many axes (or a truncated one with `Some(None)`).
    fn names_font(family: &str, fvar: Option<Option<u16>>) -> Vec<u8> {
        let u16s = |v: &[u16]| -> Vec<u8> { v.iter().flat_map(|x| x.to_be_bytes()).collect() };
        let strings: Vec<Vec<u8>> = [family, "Regular"].iter().map(|s| s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()).collect();
        let mut name = u16s(&[0, 2, 30]);
        let mut off = 0;
        for (id, st) in strings.iter().enumerate() {
            name.extend(u16s(&[3, 1, 0x409, id as u16 + 1, st.len() as u16, off]));
            off += st.len() as u16;
        }
        strings.iter().for_each(|st| name.extend(st));
        let mut tables: Vec<(&[u8; 4], Vec<u8>)> = vec![];
        match fvar {
            Some(Some(n)) => {
                // Header, then `n` axis records: `wght` pinned at 400 (min = default = max).
                let mut t = u16s(&[1, 0, 16, 2, n, 20, 0, 4 + 4 * n]);
                (0..n).for_each(|_| t.extend(b"wght".iter().copied().chain(u16s(&[0x190, 0, 0x190, 0, 0x190, 0, 0, 256]))));
                tables.push((b"fvar", t));
            }
            Some(None) => tables.push((b"fvar", u16s(&[1, 0, 16]))),
            None => {}
        }
        tables.push((b"name", name));
        let mut out = u16s(&[1, 0, tables.len() as u16, 0, 0, 0]);
        let mut offset = 12 + 16 * tables.len();
        let mut body: Vec<u8> = vec![];
        for (tag, data) in &tables {
            out.extend(*tag);
            out.extend([0u8; 4]);
            out.extend((offset as u32).to_be_bytes());
            out.extend((data.len() as u32).to_be_bytes());
            body.extend(data);
            offset += data.len();
        }
        out.extend(body);
        out
    }

    /// The scan marks a face with an `fvar` table that declares axes as variable, reading only
    /// the table directory and the fvar header; a static font, an fvar without axes and a
    /// truncated fvar are not.
    #[test]
    fn detects_variable_faces_from_fvar() {
        let v = read_faces_bytes(&names_font("Var Test", Some(Some(1))));
        assert_eq!((v.len(), v[0].family.as_str(), v[0].variable), (1, "Var Test", true), "{v:?}");
        assert!(!read_faces_bytes(&names_font("Static Test", None))[0].variable);
        assert!(!read_faces_bytes(&names_font("No Axes", Some(Some(0))))[0].variable);
        assert!(!read_faces_bytes(&names_font("Cut", Some(None)))[0].variable);
    }
}
