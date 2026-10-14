//! Document model and high-level reader for CorelDRAW files.

use crate::container::{is_cdr, riff_chunks};
use crate::cpng::{decompress_cmpr, decompress_cpng};
use crate::error::Error;
use std::collections::HashMap;

/// Scale factor from CorelDRAW internal units (0.1 µm = 1/254,000 inch) to PostScript points (1/72 inch).
const CDR_TO_PT: f64 = 72.0 / 254_000.0;

/// Resource limits applied when reading a CorelDRAW document.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_entry: usize,
    pub max_total: usize,
    pub max_decompressed: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entry: 32 << 20,        // 32 MiB
            max_total: 128 << 20,       // 128 MiB
            max_decompressed: 64 << 20, // 64 MiB
        }
    }
}

/// Metadata about a CorelDRAW layer.
#[derive(Debug, Clone)]
pub struct LayerInfo {
    pub name: String,
    pub visible: bool,
    pub locked: bool,
    pub printable: bool,
}

/// A 2D path drawing command in document points (1/72 inch, origin at top-left, Y pointing down).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathCmd {
    MoveTo(f64, f64),
    LineTo(f64, f64),
    CubicTo(f64, f64, f64, f64, f64, f64),
    ClosePath,
}

/// Resolved RGB color with components in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CdrColor {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl CdrColor {
    pub const BLACK: Self = Self { r: 0.0, g: 0.0, b: 0.0 };
    pub const WHITE: Self = Self { r: 1.0, g: 1.0, b: 1.0 };
}

/// Resolved outline/stroke attributes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CdrStroke {
    pub color: CdrColor,
    pub width: f64,
}

/// A parsed CorelDRAW vector or text object.
#[derive(Debug, Clone)]
pub struct CdrObject {
    pub layer_index: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub path: Vec<PathCmd>,
    pub fill: Option<CdrColor>,
    pub stroke: Option<CdrStroke>,
    pub text: Option<String>,
    pub font_size: f64,
}

/// A parsed CorelDRAW document.
#[derive(Debug, Clone)]
pub struct Document {
    pub width: f64,
    pub height: f64,
    pub dpi: f64,
    pub version: Option<String>,
    pub layers: Vec<LayerInfo>,
    pub objects: Vec<CdrObject>,
    pub warnings: Vec<String>,
}

/// Parse a CorelDRAW document.
pub fn read(bytes: &[u8], limits: Limits) -> Result<Document, Error> {
    if !is_cdr(bytes) {
        return Err(Error::Malformed("not a CorelDRAW document"));
    }

    if bytes.starts_with(b"RIFF") {
        let (form, chunks) = riff_chunks(bytes)?;
        let version_tag = String::from_utf8_lossy(form).to_string();

        let version_num = chunks
            .iter()
            .find(|c| &c.id == b"vrsn")
            .and_then(|c| c.data.get(0..2))
            .map(|d| u16::from_le_bytes([d[0], d[1]]));

        let ver_str = match version_num {
            Some(v) => format!("{version_tag} (build {v})"),
            None => version_tag,
        };

        // Find the non-empty compressed stream chunk `LIST [cmpr]`
        let mut decompressed: Option<(Vec<u8>, Vec<u32>)> = None;
        for cmpr_chunk in chunks.iter().filter(|c| &c.id == b"LIST" && c.data.starts_with(b"cmpr")) {
            if let Some(inner) = cmpr_chunk.data.get(4..) {
                if let Ok((dec, bs)) = decompress_cmpr(inner, limits.max_decompressed) {
                    decompressed = Some((dec, bs));
                    break;
                } else if let Ok(dec) = decompress_cpng(inner, limits.max_decompressed)
                    && !dec.is_empty()
                {
                    decompressed = Some((dec, Vec::new()));
                    break;
                }
            }
        }

        let mut width = 595.28; // Default A4
        let mut height = 841.89;
        let mut layers = Vec::new();
        let mut objects = Vec::new();
        let mut warnings = Vec::new();

        if let Some((dec, block_sizes)) = decompressed {
            let parsed = parse_cdr_stream(&dec, &block_sizes);
            if parsed.width >= 1.0 && parsed.height >= 1.0 {
                width = parsed.width;
                height = parsed.height;
            } else {
                warnings.push("using standard canvas dimensions (page bounds not found in document stream)".into());
            }
            layers = parsed.layers;
            objects = parsed.objects;
        } else {
            warnings.push("vector stream was uncompressed or could not be decompressed".into());
        }

        Ok(Document { width, height, dpi: 72.0, version: Some(ver_str), layers, objects, warnings })
    } else {
        Err(Error::Unsupported("ZIP-based CorelDRAW native vector geometry: falling back to embedded preview"))
    }
}

/// Raw object in CorelDRAW world coordinates (0.1 µm, Y pointing up) before canvas normalization.
struct RawObject {
    layer_index: usize,
    min_wx: f64,
    min_wy: f64,
    max_wx: f64,
    max_wy: f64,
    world_path: Vec<WorldPathCmd>,
    fill: Option<CdrColor>,
    stroke: Option<CdrStroke>,
    text: Option<String>,
}

#[derive(Clone, Copy)]
enum WorldPathCmd {
    MoveTo(f64, f64),
    LineTo(f64, f64),
    CubicTo(f64, f64, f64, f64, f64, f64),
    ClosePath,
}

struct ParsedStream {
    width: f64,
    height: f64,
    layers: Vec<LayerInfo>,
    objects: Vec<CdrObject>,
}

struct StreamParser<'a> {
    buf: &'a [u8],
    block_sizes: &'a [u32],
    fills: HashMap<u32, Option<CdrColor>>,
    outlines: HashMap<u32, Option<CdrStroke>>,
    mcfg_size: Option<(f64, f64)>,
    layers: Vec<LayerInfo>,
    raw_objects: Vec<RawObject>,
}

impl<'a> StreamParser<'a> {
    fn chunk_len(&self, raw_len: u32) -> usize {
        let idx = raw_len as usize;
        if !self.block_sizes.is_empty()
            && let Some(&bs) = self.block_sizes.get(idx)
        {
            return bs as usize;
        }
        idx
    }

    fn walk_top(&mut self, start: usize, end: usize) {
        let mut pos = start;
        while pos.saturating_add(8) <= end {
            let Some(hdr) = self.buf.get(pos..pos + 8) else { break };
            let cid: [u8; 4] = [hdr[0], hdr[1], hdr[2], hdr[3]];
            let raw_len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
            let bsize = self.chunk_len(raw_len);
            let ds = pos + 8;
            let de = ds.saturating_add(bsize).min(end);

            if &cid == b"LIST"
                && let Some(lt) = self.buf.get(ds..ds.saturating_add(4))
            {
                match lt {
                    b"doc " => self.walk_doc(ds + 4, de),
                    b"page" => self.walk_page(ds + 4, de),
                    _ => {}
                }
            }
            pos = ds.saturating_add(bsize).saturating_add(bsize & 1);
        }
    }

    fn walk_doc(&mut self, start: usize, end: usize) {
        let mut pos = start;
        while pos.saturating_add(8) <= end {
            let Some(hdr) = self.buf.get(pos..pos + 8) else { break };
            let cid: [u8; 4] = [hdr[0], hdr[1], hdr[2], hdr[3]];
            let raw_len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
            let bsize = self.chunk_len(raw_len);
            let ds = pos + 8;
            let de = ds.saturating_add(bsize).min(end);

            if &cid == b"LIST" {
                if let Some(lt) = self.buf.get(ds..ds.saturating_add(4))
                    && matches!(lt, b"filt" | b"filc" | b"otlt" | b"stlt")
                {
                    self.walk_doc(ds + 4, de);
                }
            } else if &cid == b"fild" {
                if let Some(data) = self.buf.get(ds..de)
                    && let Some((id, color)) = parse_fild(data)
                {
                    self.fills.insert(id, color);
                }
            } else if &cid == b"outl" {
                if let Some(data) = self.buf.get(ds..de)
                    && let Some((id, stroke)) = parse_outl(data)
                {
                    self.outlines.insert(id, stroke);
                }
            } else if &cid == b"mcfg"
                && let Some(data) = self.buf.get(ds..de)
            {
                self.mcfg_size = parse_mcfg(data);
            }
            pos = ds.saturating_add(bsize).saturating_add(bsize & 1);
        }
    }

    fn walk_page(&mut self, start: usize, end: usize) {
        let mut pos = start;
        while pos.saturating_add(8) <= end {
            let Some(hdr) = self.buf.get(pos..pos + 8) else { break };
            let cid: [u8; 4] = [hdr[0], hdr[1], hdr[2], hdr[3]];
            let raw_len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
            let bsize = self.chunk_len(raw_len);
            let ds = pos + 8;
            let de = ds.saturating_add(bsize).min(end);

            if &cid == b"LIST"
                && let Some(lt) = self.buf.get(ds..ds.saturating_add(4))
            {
                match lt {
                    b"gobj" => self.walk_page(ds + 4, de),
                    b"layr" => self.walk_layer(ds + 4, de),
                    _ => {}
                }
            }
            pos = ds.saturating_add(bsize).saturating_add(bsize & 1);
        }
    }

    fn walk_layer(&mut self, start: usize, end: usize) {
        // First inspect layer's lgob -> loda to check stype (skip Grid=0x0b and Guides=0x0c) and name.
        let mut layer_stype = 0u32;
        let mut layer_name: Option<String> = None;

        let mut pos = start;
        while pos.saturating_add(8) <= end {
            let Some(hdr) = self.buf.get(pos..pos + 8) else { break };
            let cid: [u8; 4] = [hdr[0], hdr[1], hdr[2], hdr[3]];
            let raw_len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
            let bsize = self.chunk_len(raw_len);
            let ds = pos + 8;
            let de = ds.saturating_add(bsize).min(end);

            if &cid == b"LIST"
                && self.buf.get(ds..ds.saturating_add(4)) == Some(b"lgob")
            {
                let mut p2 = ds + 4;
                while p2.saturating_add(8) <= de {
                    let Some(h2) = self.buf.get(p2..p2 + 8) else { break };
                    let c2: [u8; 4] = [h2[0], h2[1], h2[2], h2[3]];
                    let r2 = u32::from_le_bytes([h2[4], h2[5], h2[6], h2[7]]);
                    let b2 = self.chunk_len(r2);
                    let d2s = p2 + 8;
                    let d2e = d2s.saturating_add(b2).min(de);
                    if &c2 == b"loda"
                        && let Some(loda) = self.buf.get(d2s..d2e)
                    {
                        if let Some(st) = read_u32(loda, 16) {
                            layer_stype = st;
                        }
                        layer_name = extract_ascii_string(loda);
                    }
                    p2 = d2s.saturating_add(b2).saturating_add(b2 & 1);
                }
            }
            pos = ds.saturating_add(bsize).saturating_add(bsize & 1);
        }

        // Skip CorelDRAW Grid (0x0b) and Guides (0x0c) technical layers.
        if layer_stype == 0x0b || layer_stype == 0x0c {
            return;
        }

        let tentative_layer_idx = self.layers.len();
        let before_count = self.raw_objects.len();

        self.walk_objects_in_container(start, end, tentative_layer_idx);

        // Only keep layers that actually contain imported artwork objects.
        if self.raw_objects.len() > before_count {
            let name = layer_name.unwrap_or_else(|| format!("Layer {}", tentative_layer_idx + 1));
            self.layers.push(LayerInfo { name, visible: true, locked: false, printable: true });
        }
    }

    fn walk_objects_in_container(&mut self, start: usize, end: usize, layer_idx: usize) {
        let mut pos = start;
        while pos.saturating_add(8) <= end {
            let Some(hdr) = self.buf.get(pos..pos + 8) else { break };
            let cid: [u8; 4] = [hdr[0], hdr[1], hdr[2], hdr[3]];
            let raw_len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
            let bsize = self.chunk_len(raw_len);
            let ds = pos + 8;
            let de = ds.saturating_add(bsize).min(end);

            if &cid == b"LIST"
                && let Some(lt) = self.buf.get(ds..ds.saturating_add(4))
            {
                match lt {
                    b"obj " => {
                        if self.raw_objects.len() < 25_000
                            && let Some(obj) = self.parse_object(ds + 4, de, layer_idx)
                        {
                            self.raw_objects.push(obj);
                        }
                    }
                    b"grp " | b"gobj" => {
                        self.walk_objects_in_container(ds + 4, de, layer_idx);
                    }
                    _ => {}
                }
            }
            pos = ds.saturating_add(bsize).saturating_add(bsize & 1);
        }
    }

    fn parse_object(&self, start: usize, end: usize, layer_idx: usize) -> Option<RawObject> {
        let mut bbox: Option<(i32, i32, i32, i32)> = None;
        let mut loda: Option<&[u8]> = None;
        let mut trfd: Option<&[u8]> = None;
        let mut txsm: Option<&[u8]> = None;

        let mut pos = start;
        while pos.saturating_add(8) <= end {
            let hdr = self.buf.get(pos..pos + 8)?;
            let cid: [u8; 4] = [hdr[0], hdr[1], hdr[2], hdr[3]];
            let raw_len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
            let bsize = self.chunk_len(raw_len);
            let ds = pos + 8;
            let de = ds.saturating_add(bsize).min(end);

            match &cid {
                b"bbox" => {
                    if let Some(b) = self.buf.get(ds..de) {
                        bbox = parse_bbox_bytes(b);
                    }
                }
                b"txsm" => {
                    txsm = self.buf.get(ds..de);
                }
                b"LIST" if self.buf.get(ds..ds.saturating_add(4)) == Some(b"lgob") => {
                    let mut p2 = ds + 4;
                    while p2.saturating_add(8) <= de {
                        let Some(h2) = self.buf.get(p2..p2 + 8) else { break };
                        let c2: [u8; 4] = [h2[0], h2[1], h2[2], h2[3]];
                        let r2 = u32::from_le_bytes([h2[4], h2[5], h2[6], h2[7]]);
                        let b2 = self.chunk_len(r2);
                        let d2s = p2 + 8;
                        let d2e = d2s.saturating_add(b2).min(de);

                        match &c2 {
                            b"loda" => loda = self.buf.get(d2s..d2e),
                            b"bbox" if bbox.is_none() => {
                                if let Some(b) = self.buf.get(d2s..d2e) {
                                    bbox = parse_bbox_bytes(b);
                                }
                            }
                            b"txsm" if txsm.is_none() => txsm = self.buf.get(d2s..d2e),
                            b"LIST" if self.buf.get(d2s..d2s.saturating_add(4)) == Some(b"trfl") => {
                                let mut p3 = d2s + 4;
                                while p3.saturating_add(8) <= d2e {
                                    let Some(h3) = self.buf.get(p3..p3 + 8) else { break };
                                    let c3: [u8; 4] = [h3[0], h3[1], h3[2], h3[3]];
                                    let r3 = u32::from_le_bytes([h3[4], h3[5], h3[6], h3[7]]);
                                    let b3 = self.chunk_len(r3);
                                    let d3s = p3 + 8;
                                    let d3e = d3s.saturating_add(b3).min(d2e);
                                    if &c3 == b"trfd" && trfd.is_none() {
                                        trfd = self.buf.get(d3s..d3e);
                                    }
                                    p3 = d3s.saturating_add(b3).saturating_add(b3 & 1);
                                }
                            }
                            _ => {}
                        }
                        p2 = d2s.saturating_add(b2).saturating_add(b2 & 1);
                    }
                }
                _ => {}
            }
            pos = ds.saturating_add(bsize).saturating_add(bsize & 1);
        }

        let loda_bytes = loda?;
        let num_args = read_u32(loda_bytes, 4)? as usize;
        let off_args = read_u32(loda_bytes, 8)? as usize;
        let off_types = read_u32(loda_bytes, 12)? as usize;
        let stype = read_u32(loda_bytes, 16)?;

        // Skip Guide line objects (0x16 = 22)
        if stype == 0x16 || num_args > 1024 {
            return None;
        }

        let matrix = trfd.and_then(parse_trfd_matrix).unwrap_or((1.0, 0.0, 0.0, 0.0, 1.0, 0.0));

        let mut geom_arg: Option<&[u8]> = None;
        let mut fill_id: Option<u32> = None;
        let mut outl_id: Option<u32> = None;

        for i in 0..num_args {
            let start_off = read_u32(loda_bytes, off_args.saturating_add(i * 4))? as usize;
            let end_off = read_u32(loda_bytes, off_args.saturating_add((i + 1) * 4))? as usize;
            let rev_idx = num_args.checked_sub(1)?.checked_sub(i)?;
            let arg_type = read_u32(loda_bytes, off_types.saturating_add(rev_idx * 4))?;
            if start_off <= end_off
                && let Some(arg_slice) = loda_bytes.get(start_off..end_off)
            {
                match arg_type {
                    0x1e => geom_arg = Some(arg_slice),
                    0x14 => fill_id = read_u32(arg_slice, 0),
                    0x0a => outl_id = read_u32(arg_slice, 0),
                    _ => {}
                }
            }
        }

        let fill = fill_id.and_then(|id| self.fills.get(&id).copied().flatten());
        let stroke = outl_id.and_then(|id| self.outlines.get(&id).copied().flatten());

        // Artistic Text (stype == 4)
        if stype == 4 {
            let text = txsm.and_then(extract_txsm_text)?;
            let (bx0, by0, bx1, by1) = bbox?;
            let min_wx = (bx0.min(bx1)) as f64;
            let max_wx = (bx0.max(bx1)) as f64;
            let min_wy = (by0.min(by1)) as f64;
            let max_wy = (by0.max(by1)) as f64;
            return Some(RawObject {
                layer_index: layer_idx,
                min_wx,
                min_wy,
                max_wx,
                max_wy,
                world_path: Vec::new(),
                fill: fill.or(Some(CdrColor::BLACK)),
                stroke,
                text: Some(text),
            });
        }

        let geom = geom_arg?;
        let world_path = match stype {
            1 => decode_rectangle_path(geom, matrix)?,
            2 => decode_ellipse_path(geom, matrix)?,
            3 | 0x25 => decode_curve_path(geom, matrix)?,
            _ => return None,
        };

        let (min_wx, min_wy, max_wx, max_wy) = path_world_bounds(&world_path)?;
        Some(RawObject {
            layer_index: layer_idx,
            min_wx,
            min_wy,
            max_wx,
            max_wy,
            world_path,
            fill,
            stroke,
            text: None,
        })
    }
}

fn parse_cdr_stream(stream: &[u8], block_sizes: &[u32]) -> ParsedStream {
    let mut parser = StreamParser {
        buf: stream,
        block_sizes,
        fills: HashMap::new(),
        outlines: HashMap::new(),
        mcfg_size: None,
        layers: Vec::new(),
        raw_objects: Vec::new(),
    };

    parser.walk_top(0, stream.len());

    if parser.layers.is_empty() {
        parser.layers.push(LayerInfo {
            name: "Layer 1".into(),
            visible: true,
            locked: false,
            printable: true,
        });
    }

    // Compute overall world bounds across all imported objects
    let mut global_min_x = f64::INFINITY;
    let mut global_min_y = f64::INFINITY;
    let mut global_max_x = f64::NEG_INFINITY;
    let mut global_max_y = f64::NEG_INFINITY;

    for obj in &parser.raw_objects {
        if obj.min_wx.is_finite() && obj.max_wx.is_finite() && obj.min_wy.is_finite() && obj.max_wy.is_finite() {
            global_min_x = global_min_x.min(obj.min_wx);
            global_max_x = global_max_x.max(obj.max_wx);
            global_min_y = global_min_y.min(obj.min_wy);
            global_max_y = global_max_y.max(obj.max_wy);
        }
    }

    let (origin_wx, origin_wy, width, height) = if global_min_x < global_max_x && global_min_y < global_max_y {
        let w = (global_max_x - global_min_x) * CDR_TO_PT;
        let h = (global_max_y - global_min_y) * CDR_TO_PT;
        (global_min_x, global_max_y, w.max(1.0), h.max(1.0))
    } else if let Some((mw, mh)) = parser.mcfg_size {
        (-mw / 2.0, mh / 2.0, (mw * CDR_TO_PT).max(1.0), (mh * CDR_TO_PT).max(1.0))
    } else {
        (0.0, 0.0, 595.28, 841.89)
    };

    let to_pt_x = |wx: f64| (wx - origin_wx) * CDR_TO_PT;
    let to_pt_y = |wy: f64| (origin_wy - wy) * CDR_TO_PT;

    let mut objects = Vec::with_capacity(parser.raw_objects.len());
    for raw in parser.raw_objects {
        let x = to_pt_x(raw.min_wx);
        let y = to_pt_y(raw.max_wy);
        let w = (raw.max_wx - raw.min_wx) * CDR_TO_PT;
        let h = (raw.max_wy - raw.min_wy) * CDR_TO_PT;

        let mut path = Vec::with_capacity(raw.world_path.len());
        for cmd in raw.world_path {
            match cmd {
                WorldPathCmd::MoveTo(wx, wy) => path.push(PathCmd::MoveTo(to_pt_x(wx), to_pt_y(wy))),
                WorldPathCmd::LineTo(wx, wy) => path.push(PathCmd::LineTo(to_pt_x(wx), to_pt_y(wy))),
                WorldPathCmd::CubicTo(x1, y1, x2, y2, x3, y3) => {
                    path.push(PathCmd::CubicTo(
                        to_pt_x(x1),
                        to_pt_y(y1),
                        to_pt_x(x2),
                        to_pt_y(y2),
                        to_pt_x(x3),
                        to_pt_y(y3),
                    ));
                }
                WorldPathCmd::ClosePath => path.push(PathCmd::ClosePath),
            }
        }

        let num_lines = raw.text.as_deref().map(|s| s.lines().count().max(1)).unwrap_or(1);
        let font_size = if num_lines > 1 {
            (h / (num_lines as f64 * 1.18)).clamp(4.0, 72.0)
        } else if h > w * 2.5 {
            (w * 0.85).clamp(4.0, 72.0)
        } else {
            (h * 0.85).clamp(4.0, 72.0)
        };

        objects.push(CdrObject {
            layer_index: raw.layer_index,
            x,
            y,
            width: w,
            height: h,
            path,
            fill: raw.fill,
            stroke: raw.stroke,
            text: raw.text,
            font_size,
        });
    }

    ParsedStream { width, height, layers: parser.layers, objects }
}

fn read_u16(buf: &[u8], off: usize) -> Option<u16> {
    let s = buf.get(off..off.saturating_add(2))?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32(buf: &[u8], off: usize) -> Option<u32> {
    let s = buf.get(off..off.saturating_add(4))?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_i32(buf: &[u8], off: usize) -> Option<i32> {
    let s = buf.get(off..off.saturating_add(4))?;
    Some(i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_f64(buf: &[u8], off: usize) -> Option<f64> {
    let s = buf.get(off..off.saturating_add(8))?;
    Some(f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}

fn parse_bbox_bytes(b: &[u8]) -> Option<(i32, i32, i32, i32)> {
    Some((read_i32(b, 0)?, read_i32(b, 4)?, read_i32(b, 8)?, read_i32(b, 12)?))
}

fn parse_mcfg(mcfg: &[u8]) -> Option<(f64, f64)> {
    let (w, h) = if mcfg.get(0..4) == Some(&[0x14, 0x05, 0x00, 0x00]) {
        (read_i32(mcfg, 12)? as f64, read_i32(mcfg, 16)? as f64)
    } else {
        (read_i32(mcfg, 4)? as f64, read_i32(mcfg, 8)? as f64)
    };
    if w > 0.0 && h > 0.0 { Some((w, h)) } else { None }
}

/// Extract 2D affine matrix `(v0, v1, x0, v3, v4, y0)` from a `trfd` chunk.
fn parse_trfd_matrix(trfd: &[u8]) -> Option<(f64, f64, f64, f64, f64, f64)> {
    let off_args = read_u32(trfd, 8)? as usize;
    let arg0_start = read_u32(trfd, off_args)? as usize;
    let moff = if trfd.get(arg0_start..arg0_start.saturating_add(4)) == Some(&[0x14, 0x05, 0x00, 0x00]) {
        arg0_start.saturating_add(16)
    } else {
        arg0_start.saturating_add(8)
    };
    let v0 = read_f64(trfd, moff)?;
    let v1 = read_f64(trfd, moff + 8)?;
    let x0 = read_f64(trfd, moff + 16)?;
    let v3 = read_f64(trfd, moff + 24)?;
    let v4 = read_f64(trfd, moff + 32)?;
    let y0 = read_f64(trfd, moff + 40)?;
    if v0.is_finite() && v1.is_finite() && x0.is_finite() && v3.is_finite() && v4.is_finite() && y0.is_finite() {
        Some((v0, v1, x0, v3, v4, y0))
    } else {
        None
    }
}

fn xform_pt(m: (f64, f64, f64, f64, f64, f64), x: f64, y: f64) -> (f64, f64) {
    let (v0, v1, x0, v3, v4, y0) = m;
    (v0 * x + v1 * y + x0, v3 * x + v4 * y + y0)
}

fn decode_curve_path(arg: &[u8], m: (f64, f64, f64, f64, f64, f64)) -> Option<Vec<WorldPathCmd>> {
    let npts = read_u16(arg, 0)? as usize;
    if npts == 0 || npts > 100_000 {
        return None;
    }
    let flags_off = 4usize.saturating_add(npts.saturating_mul(8));
    let flags = arg.get(flags_off..flags_off.saturating_add(npts))?;

    let pt = |idx: usize| -> Option<(f64, f64)> {
        let base = 4usize.saturating_add(idx.saturating_mul(8));
        let lx = read_i32(arg, base)? as f64;
        let ly = read_i32(arg, base + 4)? as f64;
        Some(xform_pt(m, lx, ly))
    };

    let mut out = Vec::with_capacity(npts);
    let mut k = 0;
    while k < npts {
        let fl = *flags.get(k)?;
        let node_type = fl & 0xc0;
        match node_type {
            0x00 => {
                let (wx, wy) = pt(k)?;
                out.push(WorldPathCmd::MoveTo(wx, wy));
                k += 1;
            }
            0x40 => {
                let (wx, wy) = pt(k)?;
                out.push(WorldPathCmd::LineTo(wx, wy));
                if (fl & 0x08) != 0 {
                    out.push(WorldPathCmd::ClosePath);
                }
                k += 1;
            }
            0xc0 if k + 2 < npts
                && (flags.get(k + 1).copied().unwrap_or(0) & 0xc0) == 0xc0
                && (flags.get(k + 2).copied().unwrap_or(0) & 0xc0) == 0x80 =>
            {
                let (x1, y1) = pt(k)?;
                let (x2, y2) = pt(k + 1)?;
                let (x3, y3) = pt(k + 2)?;
                out.push(WorldPathCmd::CubicTo(x1, y1, x2, y2, x3, y3));
                if (flags.get(k + 2).copied().unwrap_or(0) & 0x08) != 0 {
                    out.push(WorldPathCmd::ClosePath);
                }
                k += 3;
            }
            _ => {
                let (wx, wy) = pt(k)?;
                out.push(WorldPathCmd::LineTo(wx, wy));
                if (fl & 0x08) != 0 {
                    out.push(WorldPathCmd::ClosePath);
                }
                k += 1;
            }
        }
    }

    if out.is_empty() { None } else { Some(out) }
}

fn decode_rectangle_path(arg: &[u8], m: (f64, f64, f64, f64, f64, f64)) -> Option<Vec<WorldPathCmd>> {
    let dx = read_i32(arg, 0)? as f64;
    let dy = read_i32(arg, 4)? as f64;
    let (x0, y0) = xform_pt(m, 0.0, 0.0);
    let (x1, y1) = xform_pt(m, dx, 0.0);
    let (x2, y2) = xform_pt(m, dx, dy);
    let (x3, y3) = xform_pt(m, 0.0, dy);
    Some(vec![
        WorldPathCmd::MoveTo(x0, y0),
        WorldPathCmd::LineTo(x1, y1),
        WorldPathCmd::LineTo(x2, y2),
        WorldPathCmd::LineTo(x3, y3),
        WorldPathCmd::ClosePath,
    ])
}

fn decode_ellipse_path(arg: &[u8], m: (f64, f64, f64, f64, f64, f64)) -> Option<Vec<WorldPathCmd>> {
    let dx = read_i32(arg, 0)? as f64;
    let dy = read_i32(arg, 4)? as f64;
    let cx = dx * 0.5;
    let cy = dy * 0.5;
    let rx = dx * 0.5;
    let ry = dy * 0.5;
    let k = 0.5522847498307936;
    let kx = rx * k;
    let ky = ry * k;

    let p0 = xform_pt(m, cx + rx, cy);
    let c0a = xform_pt(m, cx + rx, cy + ky);
    let c0b = xform_pt(m, cx + kx, cy + ry);
    let p1 = xform_pt(m, cx, cy + ry);
    let c1a = xform_pt(m, cx - kx, cy + ry);
    let c1b = xform_pt(m, cx - rx, cy + ky);
    let p2 = xform_pt(m, cx - rx, cy);
    let c2a = xform_pt(m, cx - rx, cy - ky);
    let c2b = xform_pt(m, cx - kx, cy - ry);
    let p3 = xform_pt(m, cx, cy - ry);
    let c3a = xform_pt(m, cx + kx, cy - ry);
    let c3b = xform_pt(m, cx + rx, cy - ky);

    Some(vec![
        WorldPathCmd::MoveTo(p0.0, p0.1),
        WorldPathCmd::CubicTo(c0a.0, c0a.1, c0b.0, c0b.1, p1.0, p1.1),
        WorldPathCmd::CubicTo(c1a.0, c1a.1, c1b.0, c1b.1, p2.0, p2.1),
        WorldPathCmd::CubicTo(c2a.0, c2a.1, c2b.0, c2b.1, p3.0, p3.1),
        WorldPathCmd::CubicTo(c3a.0, c3a.1, c3b.0, c3b.1, p0.0, p0.1),
        WorldPathCmd::ClosePath,
    ])
}

fn path_world_bounds(path: &[WorldPathCmd]) -> Option<(f64, f64, f64, f64)> {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut any = false;

    let mut include = |x: f64, y: f64| {
        if x.is_finite() && y.is_finite() {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            any = true;
        }
    };

    for &cmd in path {
        match cmd {
            WorldPathCmd::MoveTo(x, y) | WorldPathCmd::LineTo(x, y) => include(x, y),
            WorldPathCmd::CubicTo(x1, y1, x2, y2, x3, y3) => {
                include(x1, y1);
                include(x2, y2);
                include(x3, y3);
            }
            WorldPathCmd::ClosePath => {}
        }
    }

    if any { Some((min_x, min_y, max_x, max_y)) } else { None }
}

/// Convert CorelDRAW color model + 4 color bytes into `CdrColor`.
fn decode_cdr_color(model: u16, c: [u8; 4]) -> CdrColor {
    match model {
        // CMYK 0..100
        0x02 | 0x04 => {
            let cy = (c[0] as f32 / 100.0).clamp(0.0, 1.0);
            let ma = (c[1] as f32 / 100.0).clamp(0.0, 1.0);
            let ye = (c[2] as f32 / 100.0).clamp(0.0, 1.0);
            let bl = (c[3] as f32 / 100.0).clamp(0.0, 1.0);
            CdrColor {
                r: (1.0 - cy) * (1.0 - bl),
                g: (1.0 - ma) * (1.0 - bl),
                b: (1.0 - ye) * (1.0 - bl),
            }
        }
        // CMYK 0..255
        0x03 | 0x11 => {
            let cy = c[0] as f32 / 255.0;
            let ma = c[1] as f32 / 255.0;
            let ye = c[2] as f32 / 255.0;
            let bl = c[3] as f32 / 255.0;
            CdrColor {
                r: (1.0 - cy) * (1.0 - bl),
                g: (1.0 - ma) * (1.0 - bl),
                b: (1.0 - ye) * (1.0 - bl),
            }
        }
        // RGB (stored as B, G, R)
        0x01 | 0x05 => CdrColor {
            r: c[2] as f32 / 255.0,
            g: c[1] as f32 / 255.0,
            b: c[0] as f32 / 255.0,
        },
        // Grayscale (0..255)
        0x08 | 0x09 => {
            let g = c[0] as f32 / 255.0;
            CdrColor { r: g, g, b: g }
        }
        // Spot / HKS / Pantone palette color: c[0..2] = spot index, c[2..4] = tint (0..100)
        0x14 | 0x19 => {
            let spot_id = u16::from_le_bytes([c[0], c[1]]);
            let tint = (u16::from_le_bytes([c[2], c[3]]) as f32 / 100.0).clamp(0.0, 1.0);
            if tint <= 0.001 {
                CdrColor::WHITE
            } else {
                // Base spot color approximation (e.g., HKS 5 / Orange = (1.0, 0.45, 0.0))
                let (sr, sg, sb) = match spot_id {
                    1..=4 => (1.0, 0.85, 0.0),
                    5..=10 => (1.0, 0.45, 0.0),
                    11..=20 => (0.9, 0.1, 0.1),
                    _ => (0.2, 0.2, 0.2),
                };
                CdrColor {
                    r: 1.0 - tint * (1.0 - sr),
                    g: 1.0 - tint * (1.0 - sg),
                    b: 1.0 - tint * (1.0 - sb),
                }
            }
        }
        _ => CdrColor::BLACK,
    }
}

/// Parse a `fild` chunk into `(fill_id, Option<CdrColor>)`.
fn parse_fild(fild: &[u8]) -> Option<(u32, Option<CdrColor>)> {
    let fill_id = read_u32(fild, 0)?;
    // CDRD / X3+ format starts with 14 05 00 00 at offset 4..8
    if fild.get(4..8) == Some(&[0x14, 0x05, 0x00, 0x00]) {
        let ftype = read_u16(fild, 12)?;
        match ftype {
            0 => Some((fill_id, None)),
            1 => {
                let model = read_u16(fild, 27)?;
                let c = fild.get(35..39)?;
                Some((fill_id, Some(decode_cdr_color(model, [c[0], c[1], c[2], c[3]]))))
            }
            2 => {
                // Fountain fill: find first color stop
                if fild.len() >= 96
                    && let Some(model) = read_u16(fild, 80)
                    && let Some(c) = fild.get(88..92)
                {
                    Some((fill_id, Some(decode_cdr_color(model, [c[0], c[1], c[2], c[3]]))))
                } else {
                    Some((fill_id, Some(CdrColor::BLACK)))
                }
            }
            _ => Some((fill_id, Some(CdrColor::BLACK))),
        }
    } else {
        // CDR9..12 format
        let ftype = read_u16(fild, 4)?;
        match ftype {
            0 => Some((fill_id, None)),
            1 => {
                let model = read_u16(fild, 8)?;
                let c = fild.get(16..20)?;
                Some((fill_id, Some(decode_cdr_color(model, [c[0], c[1], c[2], c[3]]))))
            }
            2 => {
                // Fountain fill in CDR9: first stop color model at 48..50, color bytes at 56..60
                let model = read_u16(fild, 48)?;
                let c = fild.get(56..60)?;
                Some((fill_id, Some(decode_cdr_color(model, [c[0], c[1], c[2], c[3]]))))
            }
            _ => Some((fill_id, Some(CdrColor::BLACK))),
        }
    }
}

/// Parse an `outl` chunk into `(outl_id, Option<CdrStroke>)`.
fn parse_outl(outl: &[u8]) -> Option<(u32, Option<CdrStroke>)> {
    let outl_id = read_u32(outl, 0)?;
    if outl.len() >= 180 {
        // CDRD / X3+ format
        let flags = read_u16(outl, 76)?;
        if (flags & 0x01) != 0 {
            return Some((outl_id, None));
        }
        let raw_w = read_u32(outl, 80)? as f64;
        let model = read_u16(outl, 28)?;
        let c = outl.get(36..40)?;
        let color = decode_cdr_color(model, [c[0], c[1], c[2], c[3]]);
        let width = (raw_w * CDR_TO_PT).max(0.25);
        Some((outl_id, Some(CdrStroke { color, width })))
    } else {
        // CDR9..12 format (140 bytes)
        let flags = read_u16(outl, 4)?;
        if (flags & 0x01) != 0 {
            return Some((outl_id, None));
        }
        let raw_w = read_u32(outl, 12)? as f64;
        let model = read_u16(outl, 72)?;
        let c = outl.get(80..84)?;
        let color = decode_cdr_color(model, [c[0], c[1], c[2], c[3]]);
        let width = (raw_w * CDR_TO_PT).max(0.25);
        Some((outl_id, Some(CdrStroke { color, width })))
    }
}

/// Extract clean ASCII layer name from a `layr` `loda` chunk.
fn extract_ascii_string(loda: &[u8]) -> Option<String> {
    let body = loda.get(24..)?;
    for (idx, window) in body.windows(2).enumerate() {
        if window[0].is_ascii_alphabetic() && (window[1].is_ascii_alphanumeric() || window[1] == b' ') {
            let mut end = idx;
            while end < body.len() && body[end] != 0 && (body[end].is_ascii_graphic() || body[end] == b' ') {
                end += 1;
            }
            if end > idx + 2
                && let Some(slice) = body.get(idx..end)
                && let Ok(s) = std::str::from_utf8(slice)
            {
                let mut clean = s.trim().to_string();
                if clean.starts_with('d') && clean.len() > 1 && clean.chars().nth(1).is_some_and(|c| c.is_uppercase()) {
                    clean = clean[1..].to_string();
                }
                if !clean.is_empty() && !clean.contains("LIST") && !clean.contains("IST") {
                    return Some(clean);
                }
            }
        }
    }
    None
}

fn is_valid_cdr_text_byte(b: u8) -> bool {
    (0x20..=0x7e).contains(&b)
        || b == b'\r'
        || b == b'\n'
        || matches!(
            b,
            0xa1 | 0xb9 | 0xc6 | 0xe6 | 0xca | 0xea | 0xa3 | 0xb3 | 0xd1 | 0xf1 | 0xd3 | 0xf3 | 0x8c | 0x9c | 0x8f | 0x9f | 0xaf | 0xbf
        )
}

/// Extract text string from the end of a `txsm` chunk.
fn extract_txsm_text(txsm: &[u8]) -> Option<String> {
    // Trim trailing NUL bytes
    let mut end = txsm.len();
    while end > 0 && txsm.get(end - 1) == Some(&0) {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let mut start = end;
    while start > 0 {
        let b = *txsm.get(start - 1)?;
        if is_valid_cdr_text_byte(b) {
            start -= 1;
        } else {
            break;
        }
    }
    let cand = txsm.get(start..end)?;
    if cand.is_empty() {
        return None;
    }
    let decoded = decode_cdr_text(cand);
    if decoded.is_empty() { None } else { Some(decoded) }
}

/// Decode Latin-2 / Windows-1250 / ASCII CDR text bytes into a UTF-8 string.
fn decode_cdr_text(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\r' | b'\n' => out.push('\n'),
            0x20..=0x7e => out.push(b as char),
            0xa1 | 0xb9 => out.push(if b == 0xa1 { 'Ą' } else { 'ą' }),
            0xc6 | 0xe6 => out.push(if b == 0xc6 { 'Ć' } else { 'ć' }),
            0xca | 0xea => out.push(if b == 0xca { 'Ę' } else { 'ę' }),
            0xa3 | 0xb3 => out.push(if b == 0xa3 { 'Ł' } else { 'ł' }),
            0xd1 | 0xf1 => out.push(if b == 0xd1 { 'Ń' } else { 'ń' }),
            0xd3 | 0xf3 => out.push(if b == 0xd3 { 'Ó' } else { 'ó' }),
            0x8c | 0x9c => out.push(if b == 0x8c { 'Ś' } else { 'ś' }),
            0x8f | 0x9f => out.push(if b == 0x8f { 'Ź' } else { 'ź' }),
            0xaf | 0xbf => out.push(if b == 0xaf { 'Ż' } else { 'ż' }),
            _ => {}
        }
    }
    out.trim().to_string()
}
