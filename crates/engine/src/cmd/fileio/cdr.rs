use super::load::{err, raster_doc, raster_image};
use crate::Result;
use kurbo::{Affine, BezPath, Point, Rect};
use std::io::Cursor;
use std::sync::Arc;
use vectorcraft_cdr::PathCmd;
use vectorcraft_color::{Color, Paint};
use vectorcraft_doc::appearance::{Appearance, AppearanceItem, FillLayer, StrokeLayer};
use vectorcraft_doc::geom::FillRule;
use vectorcraft_doc::text::{CharStyle, TextObject};
use vectorcraft_doc::{Document, ImageObject, LayerColor, Node, NodeKind};
use vectorcraft_geom::PathData;

pub(super) struct Imported {
    pub doc: Document,
    pub warnings: Vec<String>,
    pub preview_only: bool,
}

pub(super) fn import(name: &str, bytes: &[u8]) -> Result<Imported> {
    match vectorcraft_cdr::read(bytes, vectorcraft_cdr::Limits::default()) {
        Ok(d) => {
            let mut warnings = d.warnings;
            let ver = d.version.as_deref().unwrap_or("CorelDRAW");
            warnings.push(format!(
                "{ver}: imported document geometry ({} × {} pt, {} layers, {} objects).",
                d.width.round(),
                d.height.round(),
                d.layers.len(),
                d.objects.len()
            ));
            let mut doc = Document::new(d.width, d.height);

            // Setup layers from CorelDRAW
            let mut layer_ids = Vec::new();
            if !d.layers.is_empty() {
                doc.layers.clear();
                for (i, layer_info) in d.layers.iter().enumerate() {
                    let id = doc.alloc_id();
                    let mut node = Node::layer(id, &layer_info.name, LayerColor::Preset((i as u8) % 9));
                    node.visible = layer_info.visible;
                    node.locked = layer_info.locked;
                    doc.layers.push(Arc::new(node));
                    layer_ids.push(id);
                }
            } else if let Some(first) = doc.layers.first() {
                layer_ids.push(first.id);
            }

            let default_layer = layer_ids.first().copied();

            // If we have native vector/text objects, insert them into the document
            if !d.objects.is_empty() {
                for obj in &d.objects {
                    let target_layer = layer_ids.get(obj.layer_index).copied().or(default_layer);
                    let id = doc.alloc_id();
                    if let Some(text) = &obj.text {
                        let fill_color = obj
                            .fill
                            .map(|c| Color::Rgb { r: c.r, g: c.g, b: c.b })
                            .unwrap_or(Color::Rgb { r: 0.0, g: 0.0, b: 0.0 });
                        let to = TextObject::point(
                            Point::new(obj.x, obj.y + obj.font_size),
                            text,
                            CharStyle { size: obj.font_size, fill: Paint::solid(fill_color), ..Default::default() },
                        );
                        let mut n = Node::new(id, NodeKind::Text(Box::new(to)));
                        n.name = Some(text.lines().next().unwrap_or(text).to_string());
                        let _ = doc.insert(target_layer, usize::MAX, n);
                    } else {
                        let pd = if !obj.path.is_empty() {
                            let mut bp = BezPath::new();
                            for &cmd in &obj.path {
                                match cmd {
                                    PathCmd::MoveTo(x, y) => bp.move_to(Point::new(x, y)),
                                    PathCmd::LineTo(x, y) => bp.line_to(Point::new(x, y)),
                                    PathCmd::CubicTo(x1, y1, x2, y2, x3, y3) => {
                                        bp.curve_to(Point::new(x1, y1), Point::new(x2, y2), Point::new(x3, y3));
                                    }
                                    PathCmd::ClosePath => bp.close_path(),
                                }
                            }
                            PathData::from_bezpath(&bp)
                        } else {
                            let rect = Rect::new(obj.x, obj.y, obj.x + obj.width, obj.y + obj.height);
                            vectorcraft_geom::shapes::rectangle(rect)
                        };

                        if pd.is_empty() {
                            continue;
                        }

                        let mut app = Appearance::default();
                        if let Some(f) = obj.fill {
                            let color = Color::Rgb { r: f.r, g: f.g, b: f.b };
                            app.items.push(AppearanceItem::Fill(FillLayer::new(Paint::solid(color))));
                        }
                        if let Some(s) = obj.stroke {
                            let color = Color::Rgb { r: s.color.r, g: s.color.g, b: s.color.b };
                            app.items.push(AppearanceItem::Stroke(StrokeLayer::new(Paint::solid(color), s.width)));
                        }
                        if app.items.is_empty() {
                            // Wireframe fallback only if object had neither fill nor stroke
                            let stroke = StrokeLayer::new(Paint::solid(Color::Rgb { r: 0.0, g: 0.0, b: 0.0 }), 0.5);
                            app.items.push(AppearanceItem::Stroke(stroke));
                        }

                        let mut n = Node::new(
                            id,
                            NodeKind::Path {
                                path: pd,
                                rule: FillRule::EvenOdd,
                                live: None,
                                clipping: false,
                                guide: false,
                            },
                        );
                        n.appearance = app.into();
                        let _ = doc.insert(target_layer, usize::MAX, n);
                    }
                }
            } else if let Some(r) = vectorcraft_cdr::preview(bytes).ok().and_then(|p| raster_image(&p.data).ok()) {
                // Fallback to preview only if no vector objects exist
                let id = doc.alloc_id();
                let sx = d.width / (r.width as f64).max(1.0);
                let sy = d.height / (r.height as f64).max(1.0);
                let xf = Affine::scale_non_uniform(sx, sy);
                let mut n = Node::new(
                    id,
                    NodeKind::Image(Box::new(ImageObject {
                        key: r.key.clone(),
                        width: r.width,
                        height: r.height,
                        xf,
                        link: None,
                        placement: Default::default(),
                    })),
                );
                n.name = Some(format!("{name} (rendered)"));
                doc.images.insert(r.key, r.blob);
                let _ = doc.insert(default_layer, 0, n);
            }

            Ok(Imported { doc, warnings, preview_only: false })
        }
        Err(native) => {
            let (doc, w) = preview(name, bytes, &native)?;
            Ok(Imported { doc, warnings: vec![w], preview_only: true })
        }
    }
}

/// The embedded preview, for files whose native document can't be read.
fn preview(name: &str, bytes: &[u8], native: &vectorcraft_cdr::Error) -> Result<(Document, String)> {
    let p = vectorcraft_cdr::preview(bytes).map_err(|e| err(format!("{native}; its embedded preview can't be read either ({e})")))?;

    let mut reader = image::ImageReader::new(Cursor::new(&p.data));
    reader = reader.with_guessed_format().map_err(err)?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(128 << 20);
    reader.limits(limits);
    let _pixels = reader.decode().map_err(err)?;
    let d = raster_doc(name, &p.data)?;
    Ok((d, format!("CorelDRAW: {native}; opened its embedded preview ({}x{} pixels).", p.width, p.height)))
}
