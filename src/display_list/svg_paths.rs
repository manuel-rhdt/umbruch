//! Converts SVG paths and their paint to page drawing commands.

use super::{DisplayItem, PathSegment, PathStroke, Point, Rect, rect_path};
use crate::style::properties::computed::{PaintColor, PixelLength};
use crate::style::properties::gradient::{BoxSize, Geometry, Resolved, ResolvedStop};
use usvg::tiny_skia_path::{self as skia, Transform};

pub(crate) fn scene(tree: &usvg::Tree) -> Option<Box<[DisplayItem]>> {
    let mut items = Vec::new();
    group(tree.root(), &mut items)?;
    Some(items.into_boxed_slice())
}

pub(crate) fn transform(t: Transform) -> [f32; 6] {
    [t.sx, t.ky, t.kx, t.sy, t.tx, t.ty]
}

fn group(group: &usvg::Group, items: &mut Vec<DisplayItem>) -> Option<()> {
    if group.opacity().get() != 1.0
        || group.blend_mode() != usvg::BlendMode::Normal
        || group.mask().is_some()
        || !group.filters().is_empty()
        || group.gouraud().is_some()
    {
        return None;
    }
    let scoped = !group.transform().is_identity() || group.clip_path().is_some();
    if scoped {
        items.push(DisplayItem::Push);
    }
    if !group.transform().is_identity() {
        items.push(DisplayItem::Transform(transform(group.transform())));
    }
    if let Some(clip) = group.clip_path() {
        add_clip(clip, items)?;
    }
    for node in group.children() {
        match node {
            usvg::Node::Path(path) => add_path(path, items)?,
            usvg::Node::Group(child) => self::group(child, items)?,
            _ => return None,
        }
    }
    if scoped {
        items.push(DisplayItem::Pop);
    }
    Some(())
}

fn add_clip(clip: &usvg::ClipPath, items: &mut Vec<DisplayItem>) -> Option<()> {
    if let Some(outer) = clip.clip_path() {
        add_clip(outer, items)?;
    }
    // A single path can be represented by the shared fill rule. Combining multiple children
    // also requires the SVG clip-union semantics.
    let mut found = None;
    clip_shape(clip.root(), clip.transform(), &mut found)?;
    let (path, even_odd) = found.unwrap_or_else(|| (rect_path(Rect::default()), false));
    items.push(DisplayItem::ClipPath { path, even_odd });
    Some(())
}

fn clip_shape(
    group: &usvg::Group,
    t: Transform,
    found: &mut Option<(Vec<PathSegment>, bool)>,
) -> Option<()> {
    if group.clip_path().is_some() {
        return None;
    }
    let t = t.pre_concat(group.transform());
    for node in group.children() {
        match node {
            usvg::Node::Path(path) if !path.is_visible() => {}
            usvg::Node::Path(path) if found.is_none() => {
                let even_odd = path
                    .fill()
                    .is_some_and(|fill| fill.rule() == usvg::FillRule::EvenOdd);
                *found = Some((segments(path.data(), t).collect(), even_odd));
            }
            usvg::Node::Group(group) => clip_shape(group, t, found)?,
            _ => return None,
        }
    }
    Some(())
}

fn add_path(path: &usvg::Path, items: &mut Vec<DisplayItem>) -> Option<()> {
    if !path.is_visible() {
        return Some(());
    }
    let stroke_first = path.paint_order() == usvg::PaintOrder::StrokeAndFill;
    for stroke in if stroke_first {
        [true, false]
    } else {
        [false, true]
    } {
        if stroke {
            if let Some(stroke) = path.stroke() {
                if stroke.dasharray().is_some() || stroke.linejoin() == usvg::LineJoin::MiterClip {
                    return None;
                }
                let color = solid_color(stroke.paint(), stroke.opacity().get())?;
                items.push(DisplayItem::StrokeColor(color));
                items.push(DisplayItem::StrokePath {
                    path: segments(path.data(), Transform::identity()).collect(),
                    style: stroke_style(stroke),
                });
            }
        } else if let Some(fill) = path.fill()
            && !is_line(path.data())
        {
            let even_odd = fill.rule() == usvg::FillRule::EvenOdd;
            let outline = segments(path.data(), Transform::identity()).collect();
            if let Some(color) = solid_color(fill.paint(), fill.opacity().get()) {
                items.push(DisplayItem::Color(color));
                items.push(DisplayItem::FillPath {
                    path: outline,
                    even_odd,
                });
            } else {
                let (gradient, matrix) = resolve_gradient(fill, path.bounding_box()?)?;
                items.push(DisplayItem::Push);
                items.push(DisplayItem::ClipPath {
                    path: outline,
                    even_odd,
                });
                if !matrix.is_identity() {
                    items.push(DisplayItem::Transform(transform(matrix)));
                }
                items.push(DisplayItem::Gradient(Box::new(gradient)));
                items.push(DisplayItem::Pop);
            }
        }
    }
    Some(())
}

fn resolve_gradient(fill: &usvg::Fill, bounds: skia::Rect) -> Option<(Resolved, Transform)> {
    let (base, geometry): (&usvg::BaseGradient, _) = match fill.paint() {
        usvg::Paint::LinearGradient(g) => {
            let start = (g.x1(), g.y1());
            let end = (g.x2(), g.y2());
            if start == end {
                return None;
            }
            (&**g, Geometry::Linear { start, end })
        }
        usvg::Paint::RadialGradient(g)
            if g.fx() == g.cx() && g.fy() == g.cy() && g.fr().get() == 0.0 =>
        {
            let radius = g.r().get();
            let geometry = Geometry::Radial {
                center: (g.cx(), g.cy()),
                radius: (radius, radius),
            };
            (&**g, geometry)
        }
        _ => return None,
    };
    if base.spread_method() != usvg::SpreadMethod::Pad {
        return None;
    }
    let stops = base
        .stops()
        .iter()
        .map(|stop| {
            let c = stop.color();
            if c.wide.is_some() {
                return None;
            }
            Some(ResolvedStop {
                offset: stop.offset().get(),
                color: [
                    f32::from(c.red) / 255.0,
                    f32::from(c.green) / 255.0,
                    f32::from(c.blue) / 255.0,
                    stop.opacity().get() * fill.opacity().get(),
                ],
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let matrix = base.transform();
    // The path clips in SVG user space. The fill bounds cover it in gradient space,
    // where an objectBoundingBox gradient can remain a reusable unit shading.
    let bounds = bounds.transform(matrix.invert()?)?;
    let paint = BoxSize {
        x: bounds.x(),
        y: bounds.y(),
        width: bounds.width(),
        height: bounds.height(),
    };
    Some((
        Resolved {
            paint,
            geometry,
            stops,
            clip: false,
            tiling: None,
        },
        matrix,
    ))
}

pub(crate) fn stroke_style(stroke: &usvg::Stroke) -> PathStroke {
    PathStroke {
        width: PixelLength(stroke.width().get()),
        cap: stroke.linecap(),
        join: stroke.linejoin(),
        miter_limit: stroke.miterlimit().get(),
    }
}

fn solid_color(paint: &usvg::Paint, opacity: f32) -> Option<PaintColor> {
    match paint {
        usvg::Paint::Color(c) if opacity == 1.0 && c.wide.is_none() => {
            Some(PaintColor::srgb(c.red, c.green, c.blue, 255))
        }
        _ => None,
    }
}

/// Converts quadratic curves to cubics while preserving the current point of each subpath.
pub(crate) fn segments(path: &skia::Path, t: Transform) -> impl Iterator<Item = PathSegment> + '_ {
    let map = move |p: skia::Point| {
        Point::new(
            PixelLength(t.sx * p.x + t.kx * p.y + t.tx),
            PixelLength(t.ky * p.x + t.sy * p.y + t.ty),
        )
    };
    let mut current = skia::Point::zero();
    let mut start = current;
    path.segments().map(move |segment| match segment {
        skia::PathSegment::MoveTo(p) => {
            current = p;
            start = p;
            PathSegment::MoveTo(map(p))
        }
        skia::PathSegment::LineTo(p) => {
            current = p;
            PathSegment::LineTo(map(p))
        }
        skia::PathSegment::QuadTo(q, p) => {
            let control = |from: skia::Point| {
                skia::Point::from_xy(
                    from.x + (q.x - from.x) * 2.0 / 3.0,
                    from.y + (q.y - from.y) * 2.0 / 3.0,
                )
            };
            let a = control(current);
            let b = control(p);
            current = p;
            PathSegment::CurveTo(map(a), map(b), map(p))
        }
        skia::PathSegment::CubicTo(a, b, p) => {
            current = p;
            PathSegment::CurveTo(map(a), map(b), map(p))
        }
        skia::PathSegment::Close => {
            current = start;
            PathSegment::Close
        }
    })
}

/// Some PDF viewers paint a hairline for a fill of a path that lies on one line.
pub(crate) fn is_line(path: &skia::Path) -> bool {
    let mut points = path.points().iter().copied();
    let Some(first) = points.next() else {
        return true;
    };
    if path.segments().any(|segment| {
        matches!(
            segment,
            skia::PathSegment::QuadTo(..) | skia::PathSegment::CubicTo(..)
        )
    }) {
        return false;
    }
    let Some(second) = points.clone().find(|p| *p != first) else {
        return true;
    };
    let (dx, dy) = (second.x - first.x, second.y - first.y);
    let length_squared = dx * dx + dy * dy;
    points.all(|p| {
        let cross = dx * (p.y - first.y) - dy * (p.x - first.x);
        cross * cross <= 1e-10 * length_squared * length_squared
    })
}
