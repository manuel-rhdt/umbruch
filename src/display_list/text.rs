//! Resolves the display list's compact text operators into positioned glyph runs.

use std::ops::Range;

use crate::layout::FontMap;
use crate::layout::components::{KernedItem, Paragraph, TextOperator};
use crate::layout::text::font::{Font, GlyphIndex};
use crate::style::properties::computed::{PaintColor, PixelLength};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionedGlyph {
    pub id: GlyphIndex,
    /// Offset in CSS pixels, relative to the run's transform. Y points down.
    pub x: PixelLength,
    pub y: PixelLength,
}

#[derive(Debug)]
pub struct PositionedGlyphRun<'text, 'scratch> {
    pub font_name: &'text str,
    pub font: &'text Font,
    pub font_size: PixelLength,
    pub color: PaintColor,
    /// Affine matrix [a, b, c, d, e, f] from run coordinates to page coordinates.
    /// Page coordinates are in CSS pixels with the origin at the top left.
    pub transform: [f64; 6],
    pub glyphs: &'scratch [PositionedGlyph],
    /// The matching indices in the original paragraph's glyph buffer, which holds the clusters.
    pub source_range: Range<usize>,
}

/// A lending iterator over the visual runs of a laid-out paragraph. It borrows the original
/// operators and resolves positions into caller-owned scratch space, one run at a time.
/// The paragraph retains its logical text, clusters and marked-content operators.
pub struct PositionedGlyphRuns<'text> {
    paragraph: &'text Paragraph,
    fonts: &'text FontMap,
    ops: std::slice::Iter<'text, TextOperator>,
    line: [f64; 6],
    pen: PixelLength,
    rise: PixelLength,
    font: Option<(&'text str, &'text Font)>,
    size: PixelLength,
    color: PaintColor,
    saw_word_spacing: bool,
}

impl<'text> PositionedGlyphRuns<'text> {
    pub fn new(
        paragraph: &'text Paragraph,
        fonts: &'text FontMap,
        initial_color: PaintColor,
    ) -> Self {
        let (x, y) = paragraph.start_pos;
        Self {
            paragraph,
            fonts,
            ops: paragraph.ops.iter(),
            line: translation(x, y),
            pen: PixelLength(0.0),
            rise: PixelLength(0.0),
            font: None,
            size: PixelLength(0.0),
            color: initial_color,
            saw_word_spacing: false,
        }
    }

    /// The current fill color, including trailing color changes once the iterator is exhausted.
    pub fn color(&self) -> PaintColor {
        self.color
    }

    /// Whether a word-spacing operator was encountered. Current layout emits spacing in the kerns.
    pub fn saw_word_spacing(&self) -> bool {
        self.saw_word_spacing
    }

    /// Resolves the next nonempty glyph run. The scratch buffer can be reused across paragraphs.
    pub fn next_run<'scratch>(
        &mut self,
        scratch: &'scratch mut Vec<PositionedGlyph>,
    ) -> Option<PositionedGlyphRun<'text, 'scratch>> {
        for op in self.ops.by_ref() {
            match *op {
                TextOperator::SetFont(ref name, size) => {
                    self.font = self
                        .fonts
                        .get(name)
                        .map(|font| (name.as_str(), font.as_ref()));
                    self.size = size;
                }
                TextOperator::SetColor(color) => self.color = color,
                TextOperator::NewLine { x_offset, leading } => {
                    let [a, b, c, d, x, y] = self.line;
                    let (dx, dy) = (f64::from(x_offset.0), f64::from(leading.0));
                    self.line[4] = a * dx + c * dy + x;
                    self.line[5] = b * dx + d * dy + y;
                    self.pen = PixelLength(0.0);
                }
                TextOperator::Embed { x, y, .. } => {
                    self.line = translation(x, y);
                    self.pen = PixelLength(0.0);
                }
                TextOperator::SetTransform(matrix) => {
                    // The stored linear part uses y-up axes. The origin is already in page pixels.
                    self.line = [
                        matrix.a, -matrix.b, -matrix.c, matrix.d, matrix.e.0, matrix.f.0,
                    ]
                    .map(f64::from);
                    self.pen = PixelLength(0.0);
                }
                TextOperator::SetRise(rise) => self.rise = rise,
                TextOperator::KernedGlyphs(range) => {
                    let Some((font_name, font)) = self.font else {
                        continue;
                    };
                    scratch.clear();
                    range.for_each_item(&self.paragraph.ops.glyphs, |item| match item {
                        KernedItem::Kern(kern) => {
                            self.pen = self.pen - font.scale_to_px(kern, self.size)
                        }
                        KernedItem::Glyphs(glyphs) => {
                            for glyph in glyphs {
                                scratch.push(PositionedGlyph {
                                    id: glyph.glyph,
                                    x: self.pen,
                                    y: -self.rise,
                                });
                                self.pen += font.scale_to_px(font.advance(glyph.glyph), self.size);
                            }
                        }
                    });
                    if !scratch.is_empty() {
                        return Some(PositionedGlyphRun {
                            font_name,
                            font,
                            font_size: self.size,
                            color: self.color,
                            transform: self.line,
                            glyphs: scratch,
                            source_range: range.start as usize..range.end as usize,
                        });
                    }
                }
                TextOperator::SetWordSpacing(_) => self.saw_word_spacing = true,
                TextOperator::BeginActualText(_)
                | TextOperator::BeginArtifact
                | TextOperator::BeginTag(_)
                | TextOperator::EndMarkedContent
                | TextOperator::ColorGlyph { .. } => {}
            }
        }
        None
    }
}

fn translation(x: PixelLength, y: PixelLength) -> [f64; 6] {
    [1.0, 0.0, 0.0, 1.0, f64::from(x.0), f64::from(y.0)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::components::{GlyphRange, KernedGlyph, TransformMatrix};
    use crate::style::properties::computed::FontWeight;
    use std::sync::Arc;

    #[test]
    fn positions_runs_across_kerns_and_translated_baselines() {
        let font = Arc::new(Font::bundled(&FontWeight(400.0)));
        let name = font.postscript_name();
        let size = PixelLength(10.0);
        let glyph = GlyphIndex(36);
        let advance = font.scale_to_px(font.advance(glyph), size);
        let fonts = FontMap::from([(name.clone(), font)]);
        let mut paragraph = Paragraph::new(PixelLength(10.0), PixelLength(20.0));
        let draw = |start, end, trailing_kern| {
            TextOperator::KernedGlyphs(GlyphRange {
                start,
                end,
                trailing_kern,
            })
        };
        let red = PaintColor::srgb(255, 0, 0, 255);
        paragraph.ops.extend([
            TextOperator::SetFont(name, size),
            draw(0, 1, 3000),
            draw(1, 1, -5000),
            TextOperator::SetColor(red),
            draw(1, 2, 0),
            TextOperator::SetTransform(TransformMatrix {
                a: 0.0,
                b: -1.0,
                c: 1.0,
                d: 0.0,
                e: PixelLength(40.0),
                f: PixelLength(50.0),
            }),
            TextOperator::NewLine {
                x_offset: PixelLength(2.0),
                leading: PixelLength(3.0),
            },
            TextOperator::SetRise(PixelLength(4.0)),
            draw(2, 3, 0),
            TextOperator::SetColor(PaintColor::transparent()),
        ]);
        paragraph.ops.glyphs.extend([
            KernedGlyph {
                glyph,
                kern: 1000,
                cluster: 0,
            },
            KernedGlyph {
                glyph,
                kern: 0,
                cluster: 2,
            },
            KernedGlyph {
                glyph,
                kern: 0,
                cluster: 5,
            },
        ]);

        let mut runs = PositionedGlyphRuns::new(&paragraph, &fonts, PaintColor::srgb(0, 0, 0, 255));
        let mut scratch = Vec::new();
        let first = runs.next_run(&mut scratch).unwrap();
        assert_eq!(first.transform, [1.0, 0.0, 0.0, 1.0, 10.0, 20.0]);
        assert_eq!(first.glyphs[0].x, PixelLength(-1.0));

        let second = runs.next_run(&mut scratch).unwrap();
        assert_eq!(second.color, red);
        assert!((second.glyphs[0].x.0 - (advance.0 + 1.0)).abs() < 0.00001);
        assert_eq!(second.source_range, 1..2);
        assert_eq!(paragraph.ops.glyphs[second.source_range.start].cluster, 2);

        let third = runs.next_run(&mut scratch).unwrap();
        assert_eq!(third.transform, [0.0, 1.0, -1.0, 0.0, 37.0, 52.0]);
        assert_eq!(
            third.glyphs[0],
            PositionedGlyph {
                id: glyph,
                x: PixelLength(0.0),
                y: PixelLength(-4.0)
            }
        );

        assert!(runs.next_run(&mut scratch).is_none());
        assert_eq!(runs.color(), PaintColor::transparent());
    }
}
