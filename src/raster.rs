//! Experimental PNG output from the same pages and shaped text that the PDF writer uses.

use std::collections::{BTreeSet, HashMap};
use std::io::Cursor;
use std::sync::Arc;

use vello_cpu::kurbo::{Affine, BezPath, Point, Rect, Stroke, Vec2};
use vello_cpu::peniko::{Blob, Color, Fill, FontData, ImageAlphaType};
use vello_cpu::{Glyph, Image, ImageSource, Pixmap, RenderContext, Resources};

use crate::display_list::text::{PositionedGlyph, PositionedGlyphRuns};
use crate::display_list::{DisplayItem, PathSegment, PrintPages, Rect as PageRect};
use crate::engine::{Error, Result};
use crate::image::LoadedImage;
use crate::layout::FontMap;
use crate::layout::components::Paragraph;
use crate::style::properties::computed::{PaintColor, PixelLength};

// An RGBA target at this limit uses 64 MiB before allocating any renderer buffers.
const MAX_PIXELS: u64 = 16 * 1024 * 1024;

pub(crate) fn render_png(
    mut pages: PrintPages,
    number: usize,
    crop: bool,
    dpi: u32,
) -> Result<Vec<u8>> {
    let count = pages.pages.len();
    let page = number
        .checked_sub(1)
        .and_then(|index| pages.pages.get_mut(index))
        .ok_or_else(|| {
            Error::BadRequest(format!("PNG page {number} is out of range ({count} pages)"))
        })?;
    if dpi == 0 {
        return Err(Error::BadRequest(
            "PNG resolution must be greater than zero".into(),
        ));
    }

    let bounds = if crop {
        page.content_rect
    } else {
        PageRect {
            width: page.width,
            height: page.height,
            ..PageRect::default()
        }
    };
    let scale = f64::from(dpi) / 96.0;
    let dimension = |length: PixelLength| {
        let pixels = (f64::from(length.0) * scale).ceil();
        (0.0..=f64::from(u16::MAX))
            .contains(&pixels)
            .then_some(pixels.max(1.0) as u16)
    };
    let too_large = || Error::BadRequest("PNG page exceeds the raster backend's size limit".into());
    let width = dimension(bounds.width).ok_or_else(too_large)?;
    let height = dimension(bounds.height).ok_or_else(too_large)?;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(too_large());
    }

    let transform =
        Affine::scale(scale) * Affine::translate((-f64::from(bounds.x.0), -f64::from(bounds.y.0)));
    let mut painter = Painter {
        ctx: RenderContext::new(width, height),
        resources: Resources::new(),
        fonts: &pages.font_map,
        font_data: HashMap::new(),
        glyphs: Vec::new(),
        images: HashMap::new(),
        unsupported: BTreeSet::new(),
    };
    painter.paint(page.display_list.items(), transform)?;
    for item in painter.unsupported {
        log::warn!("PNG output skipped {item}");
    }

    let mut pixmap = Pixmap::new(width, height);
    painter.ctx.flush();
    painter
        .ctx
        .render_with(&mut pixmap, &mut painter.resources, Default::default());
    let pixels = pixmap.take_rgba8(ImageAlphaType::Alpha);
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, u32::from(width), u32::from(height));
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    let pixels_per_meter = (f64::from(dpi) / 0.0254).round() as u32;
    encoder.set_pixel_dims(Some(png::PixelDimensions {
        xppu: pixels_per_meter,
        yppu: pixels_per_meter,
        unit: png::Unit::Meter,
    }));
    let encode = || -> std::result::Result<(), png::EncodingError> {
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&pixels)?;
        writer.finish()
    };
    encode().map_err(|error| Error::Render(format!("could not encode PNG: {error}")))?;
    Ok(bytes)
}

#[derive(Clone, Copy)]
struct PaintState {
    transform: Affine,
    fill: PaintColor,
    stroke: PaintColor,
    clips: usize,
}

struct Painter<'a> {
    ctx: RenderContext,
    resources: Resources,
    fonts: &'a FontMap,
    font_data: HashMap<String, FontData>,
    glyphs: Vec<PositionedGlyph>,
    images: HashMap<*const LoadedImage, Arc<Pixmap>>,
    unsupported: BTreeSet<&'static str>,
}

impl Painter<'_> {
    fn paint<'a>(
        &mut self,
        items: impl Iterator<Item = &'a DisplayItem>,
        transform: Affine,
    ) -> Result<()> {
        let mut state = PaintState {
            transform,
            fill: PaintColor::srgb(0, 0, 0, 255),
            stroke: PaintColor::srgb(0, 0, 0, 255),
            clips: 0,
        };
        let mut saved = Vec::new();
        self.ctx.set_transform(transform);

        for item in items {
            match item {
                DisplayItem::Push => saved.push(state),
                DisplayItem::Pop | DisplayItem::EndGroup => {
                    let previous = saved.pop().expect("balanced display-list graphics state");
                    for _ in previous.clips..state.clips {
                        self.ctx.pop_clip();
                    }
                    if matches!(item, DisplayItem::EndGroup) {
                        self.ctx.pop_layer();
                    }
                    state = previous;
                    self.ctx.set_transform(state.transform);
                }
                DisplayItem::Translate(x, y) => {
                    state.transform *= Affine::translate((f64::from(x.0), f64::from(y.0)));
                    self.ctx.set_transform(state.transform);
                }
                DisplayItem::Transform(matrix) => {
                    state.transform *= Affine::new(matrix.map(f64::from));
                    self.ctx.set_transform(state.transform);
                }
                DisplayItem::Color(fill) => state.fill = *fill,
                DisplayItem::StrokeColor(stroke) => state.stroke = *stroke,
                DisplayItem::FilledRect(bounds) => {
                    self.ctx.set_paint(color(state.fill));
                    self.ctx.fill_rect(&rect(*bounds));
                }
                DisplayItem::FillPath { path, even_odd } => {
                    self.ctx.set_paint(color(state.fill));
                    self.ctx.set_fill_rule(fill_rule(*even_odd));
                    self.ctx.fill_path(&bezier(path));
                }
                DisplayItem::ClipPath { path, even_odd } => {
                    self.ctx.set_fill_rule(fill_rule(*even_odd));
                    self.ctx.push_clip_path(&bezier(path));
                    state.clips += 1;
                }
                DisplayItem::LineString { points, line_width } => {
                    if let Some((first, rest)) = points.split_first() {
                        let mut path = BezPath::new();
                        path.move_to(point(*first));
                        for next in rest {
                            path.line_to(point(*next));
                        }
                        self.ctx.set_paint(color(state.stroke));
                        self.ctx.set_stroke(Stroke::new(f64::from(line_width.0)));
                        self.ctx.stroke_path(&path);
                    }
                }
                DisplayItem::BeginGroup { opacity, .. } => {
                    saved.push(state);
                    self.ctx.push_opacity_layer(*opacity);
                }
                DisplayItem::Paragraph(paragraph) => self.paragraph(paragraph, &mut state)?,
                // Glifo renders COLR glyphs as part of the glyph run.
                DisplayItem::ColorGlyph(_) => {}
                DisplayItem::Image { rect, image } => self.image(*rect, image)?,
                DisplayItem::Gradient(_) => {
                    self.unsupported.insert("gradients");
                }
                DisplayItem::BlendGroup(_) => {
                    self.unsupported.insert("background blend groups");
                }
            }
        }
        Ok(())
    }

    fn paragraph(&mut self, paragraph: &Paragraph, state: &mut PaintState) -> Result<()> {
        let mut runs = PositionedGlyphRuns::new(paragraph, self.fonts, state.fill);
        while let Some(run) = runs.next_run(&mut self.glyphs) {
            let data = self
                .font_data
                .entry(run.font_name.to_owned())
                .or_insert_with(|| FontData::new(Blob::new(run.font.data.clone()), run.font.index));
            let glyphs = run.glyphs.iter().map(|glyph| Glyph {
                id: glyph.id.0,
                x: glyph.x.0,
                y: glyph.y.0,
            });
            self.ctx
                .set_transform(state.transform * Affine::new(run.transform));
            self.ctx.set_fill_rule(Fill::NonZero);
            self.ctx.set_paint(color(run.color));
            self.ctx
                .glyph_run(&mut self.resources, data)
                .font_size(run.font_size.0)
                .hint(false)
                .fill_glyphs(glyphs)
                .map_err(|error| Error::Render(format!("could not rasterize text: {error}")))?;
        }
        if runs.saw_word_spacing() {
            self.unsupported.insert("text word-spacing operators");
        }
        state.fill = runs.color();
        self.ctx.set_transform(state.transform);
        Ok(())
    }

    fn image(&mut self, bounds: PageRect, image: &LoadedImage) -> Result<()> {
        let LoadedImage::Raster(raster) = image else {
            self.unsupported.insert("SVG images");
            return Ok(());
        };
        if !raster.data.starts_with(b"\x89PNG\r\n\x1a\n") {
            self.unsupported.insert("JPEG images");
            return Ok(());
        }
        if u64::from(raster.width) * u64::from(raster.height) > MAX_PIXELS {
            return Err(Error::Render(
                "PNG image exceeds the raster backend's size limit".into(),
            ));
        }
        let pixmap = match self.images.entry(image) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => entry.insert(Arc::new(
                Pixmap::from_png(Cursor::new(&raster.data)).map_err(|error| {
                    Error::Render(format!("could not decode PNG image: {error}"))
                })?,
            )),
        };
        let bounds = rect(bounds);
        self.ctx.set_paint_transform(
            Affine::translate(Vec2::new(bounds.x0, bounds.y0))
                * Affine::scale_non_uniform(
                    bounds.width() / f64::from(raster.width),
                    bounds.height() / f64::from(raster.height),
                ),
        );
        self.ctx.set_paint(Image {
            image: ImageSource::Pixmap(pixmap.clone()),
            sampler: Default::default(),
        });
        self.ctx.fill_rect(&bounds);
        self.ctx.set_paint_transform(Affine::IDENTITY);
        Ok(())
    }
}

fn color(value: PaintColor) -> Color {
    let c = value.clip_to_srgb();
    Color::from_rgba8(c.red, c.green, c.blue, c.alpha)
}

fn rect(bounds: PageRect) -> Rect {
    Rect::new(
        f64::from(bounds.x.0),
        f64::from(bounds.y.0),
        f64::from(bounds.xmax().0),
        f64::from(bounds.ymax().0),
    )
}

fn point(value: crate::display_list::Point) -> Point {
    Point::new(f64::from(value.x.0), f64::from(value.y.0))
}

fn fill_rule(even_odd: bool) -> Fill {
    if even_odd {
        Fill::EvenOdd
    } else {
        Fill::NonZero
    }
}

fn bezier(segments: &[PathSegment]) -> BezPath {
    let mut path = BezPath::new();
    for segment in segments {
        match *segment {
            PathSegment::MoveTo(p) => path.move_to(point(p)),
            PathSegment::LineTo(p) => path.line_to(point(p)),
            PathSegment::CurveTo(p1, p2, p3) => path.curve_to(point(p1), point(p2), point(p3)),
            PathSegment::Close => path.close_path(),
        }
    }
    path
}
