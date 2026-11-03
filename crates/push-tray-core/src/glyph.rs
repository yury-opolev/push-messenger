//! The tray glyph from `assets/glyph.svg` and a small rasterizer for it: an SVG path subset
//! (`M L H V A Z`, absolute and relative) flattened into polylines, filled (non-zero
//! winding) and stroked (round joins) with supersampling, in a 32×32 view box.

use std::f64::consts::PI;
use std::sync::OnceLock;

/// The glyph's SVG source: an outlined bubble and a solid bubble.
pub const SOURCE: &str = include_str!("../assets/glyph.svg");

/// The view box's side: shapes use 0..32 whatever the image size.
pub const VIEW_BOX: f64 = 32.0;

/// A point in view box units.
pub type Point = (f64, f64);

/// Path data or the SVG source could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} at byte {at}")]
pub struct PathError {
    pub at: usize,
    pub message: &'static str,
}

/// A polyline; `closed` when the path closed it with `Z`.
#[derive(Debug, Clone, PartialEq)]
pub struct Subpath {
    pub points: Vec<Point>,
    pub closed: bool,
}

/// An axis-aligned box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Bounds {
    fn of(points: impl IntoIterator<Item = Point>) -> Option<Bounds> {
        points.into_iter().fold(None, |bounds, (x, y)| {
            Some(match bounds {
                None => Bounds {
                    min_x: x,
                    min_y: y,
                    max_x: x,
                    max_y: y,
                },
                Some(b) => Bounds {
                    min_x: b.min_x.min(x),
                    min_y: b.min_y.min(y),
                    max_x: b.max_x.max(x),
                    max_y: b.max_y.max(y),
                },
            })
        })
    }

    /// The box grown by `margin` on every side.
    pub fn grow(self, margin: f64) -> Bounds {
        Bounds {
            min_x: self.min_x - margin,
            min_y: self.min_y - margin,
            max_x: self.max_x + margin,
            max_y: self.max_y + margin,
        }
    }

    fn overlaps(&self, other: &Bounds) -> bool {
        self.min_x <= other.max_x
            && other.min_x <= self.max_x
            && self.min_y <= other.max_y
            && other.min_y <= self.max_y
    }
}

/// A path's outline as polylines.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shape {
    pub subpaths: Vec<Subpath>,
}

impl Shape {
    /// Parses SVG path data (`M L H V A Z`, upper and lower case).
    pub fn parse(d: &str) -> Result<Shape, PathError> {
        PathParser::new(d).run()
    }

    /// The box around every point; `None` for an empty shape.
    pub fn bounds(&self) -> Option<Bounds> {
        Bounds::of(
            self.subpaths
                .iter()
                .flat_map(|sub| sub.points.iter().copied()),
        )
    }

    /// The shape scaled by `scale`, then moved by (`dx`, `dy`).
    pub fn transformed(&self, scale: f64, dx: f64, dy: f64) -> Shape {
        Shape {
            subpaths: self
                .subpaths
                .iter()
                .map(|sub| Subpath {
                    points: sub
                        .points
                        .iter()
                        .map(|(x, y)| (x * scale + dx, y * scale + dy))
                        .collect(),
                    closed: sub.closed,
                })
                .collect(),
        }
    }

    /// The outline's segments; `close_all` closes open subpaths too (for filling).
    fn segments(&self, close_all: bool) -> Vec<Segment> {
        let mut segments = Vec::new();
        for sub in &self.subpaths {
            for pair in sub.points.windows(2) {
                segments.push(Segment::new(pair[0], pair[1]));
            }
            if let (Some(&first), Some(&last)) = (sub.points.first(), sub.points.last()) {
                if (sub.closed || close_all) && first != last {
                    segments.push(Segment::new(last, first));
                }
                if sub.points.len() == 1 {
                    segments.push(Segment::new(first, first));
                }
            }
        }
        segments
    }
}

struct PathParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    subpaths: Vec<Subpath>,
    points: Vec<Point>,
    current: Point,
    start: Point,
}

impl<'a> PathParser<'a> {
    fn new(d: &'a str) -> Self {
        PathParser {
            bytes: d.as_bytes(),
            pos: 0,
            subpaths: Vec::new(),
            points: Vec::new(),
            current: (0.0, 0.0),
            start: (0.0, 0.0),
        }
    }

    fn error(&self, message: &'static str) -> PathError {
        PathError {
            at: self.pos,
            message,
        }
    }

    fn skip_separators(&mut self) {
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_whitespace() || self.bytes[self.pos] == b',')
        {
            self.pos += 1;
        }
    }

    fn number(&mut self) -> Result<f64, PathError> {
        self.skip_separators();
        let begin = self.pos;
        let bytes = self.bytes;
        let mut at = self.pos;
        if at < bytes.len() && (bytes[at] == b'-' || bytes[at] == b'+') {
            at += 1;
        }
        let digits = |at: &mut usize| {
            let from = *at;
            while *at < bytes.len() && bytes[*at].is_ascii_digit() {
                *at += 1;
            }
            *at > from
        };
        let mut any = digits(&mut at);
        if at < bytes.len() && bytes[at] == b'.' {
            at += 1;
            any |= digits(&mut at);
        }
        if !any {
            return Err(self.error("expected a number"));
        }
        if at < bytes.len() && (bytes[at] == b'e' || bytes[at] == b'E') {
            let mut exponent = at + 1;
            if exponent < bytes.len() && (bytes[exponent] == b'-' || bytes[exponent] == b'+') {
                exponent += 1;
            }
            if digits(&mut exponent) {
                at = exponent;
            }
        }
        self.pos = at;
        std::str::from_utf8(&bytes[begin..at])
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| self.error("bad number"))
    }

    fn flag(&mut self) -> Result<bool, PathError> {
        self.skip_separators();
        let flag = match self.bytes.get(self.pos) {
            Some(b'0') => false,
            Some(b'1') => true,
            _ => return Err(self.error("expected a flag")),
        };
        self.pos += 1;
        Ok(flag)
    }

    fn pair(&mut self) -> Result<Point, PathError> {
        Ok((self.number()?, self.number()?))
    }

    fn finish_subpath(&mut self, closed: bool) {
        if !self.points.is_empty() {
            self.subpaths.push(Subpath {
                points: std::mem::take(&mut self.points),
                closed,
            });
        }
    }

    fn line_to(&mut self, to: Point) {
        if self.points.is_empty() {
            self.points.push(self.current);
        }
        self.points.push(to);
        self.current = to;
    }

    fn run(mut self) -> Result<Shape, PathError> {
        let mut command: Option<u8> = None;
        loop {
            self.skip_separators();
            let Some(&next) = self.bytes.get(self.pos) else {
                break;
            };
            let letter = if next.is_ascii_alphabetic() {
                self.pos += 1;
                next
            } else {
                match command {
                    Some(b'Z' | b'z') | None => return Err(self.error("expected a command")),
                    Some(letter) => letter,
                }
            };
            let relative = letter.is_ascii_lowercase();
            let (cx, cy) = self.current;
            let base = |(x, y): Point| if relative { (x + cx, y + cy) } else { (x, y) };
            command = Some(letter);
            match letter.to_ascii_uppercase() {
                b'M' => {
                    let to = base(self.pair()?);
                    self.finish_subpath(false);
                    self.points.push(to);
                    self.current = to;
                    self.start = to;
                    command = Some(if relative { b'l' } else { b'L' });
                }
                b'L' => {
                    let to = base(self.pair()?);
                    self.line_to(to);
                }
                b'H' => {
                    let x = self.number()?;
                    self.line_to((if relative { x + cx } else { x }, cy));
                }
                b'V' => {
                    let y = self.number()?;
                    self.line_to((cx, if relative { y + cy } else { y }));
                }
                b'A' => {
                    let rx = self.number()?;
                    let ry = self.number()?;
                    let rotation = self.number()?;
                    let large = self.flag()?;
                    let sweep = self.flag()?;
                    let to = base(self.pair()?);
                    if self.points.is_empty() {
                        self.points.push(self.current);
                    }
                    let points = arc_points(self.current, (rx, ry), rotation, large, sweep, to);
                    self.points.extend(points);
                    self.current = to;
                }
                b'Z' => {
                    self.finish_subpath(true);
                    self.current = self.start;
                }
                _ => return Err(self.error("unsupported command")),
            }
        }
        self.finish_subpath(false);
        Ok(Shape {
            subpaths: self.subpaths,
        })
    }
}

fn angle_between(u: Point, v: Point) -> f64 {
    (u.0 * v.1 - u.1 * v.0).atan2(u.0 * v.0 + u.1 * v.1)
}

/// The points after `from` along an SVG elliptical arc to `to` (SVG 1.1, F.6.5), at most
/// π/16 apart in angle.
fn arc_points(
    from: Point,
    (rx, ry): (f64, f64),
    rotation_degrees: f64,
    large: bool,
    sweep: bool,
    to: Point,
) -> Vec<Point> {
    if from == to {
        return Vec::new();
    }
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    if rx == 0.0 || ry == 0.0 {
        return vec![to];
    }
    let (sin, cos) = rotation_degrees.to_radians().sin_cos();
    let (hx, hy) = ((from.0 - to.0) / 2.0, (from.1 - to.1) / 2.0);
    let x1 = cos * hx + sin * hy;
    let y1 = -sin * hx + cos * hy;
    let lambda = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
    if lambda > 1.0 {
        rx *= lambda.sqrt();
        ry *= lambda.sqrt();
    }
    let numerator = rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1;
    let denominator = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    let sign = if large == sweep { -1.0 } else { 1.0 };
    let coefficient = sign * (numerator / denominator).max(0.0).sqrt();
    let (ccx, ccy) = (coefficient * rx * y1 / ry, -coefficient * ry * x1 / rx);
    let centre = (
        cos * ccx - sin * ccy + (from.0 + to.0) / 2.0,
        sin * ccx + cos * ccy + (from.1 + to.1) / 2.0,
    );
    let u = ((x1 - ccx) / rx, (y1 - ccy) / ry);
    let v = ((-x1 - ccx) / rx, (-y1 - ccy) / ry);
    let theta = angle_between((1.0, 0.0), u);
    let mut delta = angle_between(u, v);
    if !sweep && delta > 0.0 {
        delta -= 2.0 * PI;
    } else if sweep && delta < 0.0 {
        delta += 2.0 * PI;
    }
    let steps = ((delta.abs() / (PI / 16.0)).ceil() as usize).max(1);
    let mut points: Vec<Point> = (1..steps)
        .map(|step| {
            let t = theta + delta * step as f64 / steps as f64;
            let (ex, ey) = (rx * t.cos(), ry * t.sin());
            (
                cos * ex - sin * ey + centre.0,
                sin * ex + cos * ey + centre.1,
            )
        })
        .collect();
    points.push(to);
    points
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    a: Point,
    b: Point,
    bounds: Bounds,
}

impl Segment {
    fn new(a: Point, b: Point) -> Self {
        Segment {
            a,
            b,
            bounds: Bounds::of([a, b]).unwrap_or(Bounds {
                min_x: a.0,
                min_y: a.1,
                max_x: a.0,
                max_y: a.1,
            }),
        }
    }

    fn distance_squared(&self, (px, py): Point) -> f64 {
        let (dx, dy) = (self.b.0 - self.a.0, self.b.1 - self.a.1);
        let length = dx * dx + dy * dy;
        let t = if length == 0.0 {
            0.0
        } else {
            (((px - self.a.0) * dx + (py - self.a.1) * dy) / length).clamp(0.0, 1.0)
        };
        let (nx, ny) = (self.a.0 + t * dx - px, self.a.1 + t * dy - py);
        nx * nx + ny * ny
    }

    /// The winding contribution of a ray from `p` towards +x.
    fn winding(&self, (px, py): Point) -> i32 {
        let ((ax, ay), (bx, by)) = (self.a, self.b);
        let side = (bx - ax) * (py - ay) - (px - ax) * (by - ay);
        if ay <= py && by > py && side > 0.0 {
            1
        } else if by <= py && ay > py && side < 0.0 {
            -1
        } else {
            0
        }
    }
}

/// An RGBA image being drawn, in view box units scaled to its size.
#[derive(Debug, Clone)]
pub struct Canvas {
    size: u32,
    /// Premultiplied RGBA, 0..1.
    pixels: Vec<[f64; 4]>,
    /// Subsample offsets within a pixel, in view box units.
    offsets: Vec<Point>,
    pixel: f64,
}

impl Canvas {
    /// A transparent `size`×`size` image.
    pub fn new(size: u32) -> Self {
        let side = size as usize;
        let pixel = if size == 0 {
            0.0
        } else {
            VIEW_BOX / f64::from(size)
        };
        let grid = if size <= 32 { 8 } else { 4 };
        let offsets = (0..grid)
            .flat_map(|row| {
                (0..grid).map(move |column| {
                    (
                        (f64::from(column) + 0.5) / f64::from(grid) * pixel,
                        (f64::from(row) + 0.5) / f64::from(grid) * pixel,
                    )
                })
            })
            .collect();
        Canvas {
            size,
            pixels: vec![[0.0; 4]; side * side],
            offsets,
            pixel,
        }
    }

    /// Calls `apply` with each pixel's coverage (0..1] within `bounds`, `inside` deciding
    /// for the pixel's box and a subsample point.
    fn cover<P>(
        &mut self,
        bounds: Bounds,
        mut prepare: impl FnMut(&Bounds) -> Option<P>,
        inside: impl Fn(&P, Point) -> bool,
        apply: impl Fn(&mut [f64; 4], f64),
    ) {
        if self.size == 0 {
            return;
        }
        let last = f64::from(self.size - 1);
        let range = |low: f64, high: f64| {
            let first = (low / self.pixel).floor().clamp(0.0, last) as usize;
            let end = (high / self.pixel).floor().clamp(0.0, last) as usize;
            first..=end
        };
        if bounds.max_x < 0.0
            || bounds.max_y < 0.0
            || bounds.min_x > VIEW_BOX
            || bounds.min_y > VIEW_BOX
        {
            return;
        }
        let side = self.size as usize;
        let samples = self.offsets.len() as f64;
        for row in range(bounds.min_y, bounds.max_y) {
            for column in range(bounds.min_x, bounds.max_x) {
                let (x0, y0) = (column as f64 * self.pixel, row as f64 * self.pixel);
                let cell = Bounds {
                    min_x: x0,
                    min_y: y0,
                    max_x: x0 + self.pixel,
                    max_y: y0 + self.pixel,
                };
                let Some(prepared) = prepare(&cell) else {
                    continue;
                };
                let hits = self
                    .offsets
                    .iter()
                    .filter(|(dx, dy)| inside(&prepared, (x0 + dx, y0 + dy)))
                    .count();
                if hits > 0 {
                    apply(&mut self.pixels[row * side + column], hits as f64 / samples);
                }
            }
        }
    }

    fn paint<P>(
        &mut self,
        bounds: Bounds,
        prepare: impl FnMut(&Bounds) -> Option<P>,
        inside: impl Fn(&P, Point) -> bool,
        color: [u8; 3],
        opacity: f64,
    ) {
        let rgb = color.map(|channel| f64::from(channel) / 255.0);
        self.cover(bounds, prepare, inside, |pixel, coverage| {
            let alpha = coverage * opacity;
            for channel in 0..3 {
                pixel[channel] = rgb[channel] * alpha + pixel[channel] * (1.0 - alpha);
            }
            pixel[3] = alpha + pixel[3] * (1.0 - alpha);
        });
    }

    /// Fills `shape` (non-zero winding; open subpaths are closed).
    pub fn fill_shape(&mut self, shape: &Shape, color: [u8; 3], opacity: f64) {
        let Some(bounds) = shape.bounds() else {
            return;
        };
        let segments = shape.segments(true);
        self.paint(
            bounds,
            |cell| {
                let near: Vec<Segment> = segments
                    .iter()
                    .filter(|s| {
                        s.bounds.max_y >= cell.min_y
                            && s.bounds.min_y <= cell.max_y
                            && s.bounds.max_x >= cell.min_x
                    })
                    .copied()
                    .collect();
                (!near.is_empty()).then_some(near)
            },
            |near, point| near.iter().map(|s| s.winding(point)).sum::<i32>() != 0,
            color,
            opacity,
        );
    }

    /// Strokes `shape`'s outline `width` wide (round joins and caps).
    pub fn stroke_shape(&mut self, shape: &Shape, width: f64, color: [u8; 3], opacity: f64) {
        let Some(bounds) = shape.bounds() else {
            return;
        };
        let half = width / 2.0;
        let limit = half * half;
        let segments = shape.segments(false);
        self.paint(
            bounds.grow(half),
            |cell| {
                let reach = cell.grow(half);
                let near: Vec<Segment> = segments
                    .iter()
                    .filter(|s| s.bounds.overlaps(&reach))
                    .copied()
                    .collect();
                (!near.is_empty()).then_some(near)
            },
            |near, point| near.iter().any(|s| s.distance_squared(point) <= limit),
            color,
            opacity,
        );
    }

    /// Fills the circle at (`cx`, `cy`) of `radius`.
    pub fn fill_circle(&mut self, (cx, cy): Point, radius: f64, color: [u8; 3], opacity: f64) {
        let bounds = Bounds::of([(cx, cy)]).map(|b| b.grow(radius));
        if let Some(bounds) = bounds {
            self.paint(
                bounds,
                |_| Some(()),
                |(), point| within_circle(point, (cx, cy), radius),
                color,
                opacity,
            );
        }
    }

    /// Makes the circle at (`cx`, `cy`) of `radius` transparent.
    pub fn clear_circle(&mut self, (cx, cy): Point, radius: f64) {
        let bounds = Bounds::of([(cx, cy)]).map(|b| b.grow(radius));
        if let Some(bounds) = bounds {
            self.cover(
                bounds,
                |_| Some(()),
                |(), point| within_circle(point, (cx, cy), radius),
                |pixel, coverage| {
                    for channel in pixel.iter_mut() {
                        *channel *= 1.0 - coverage;
                    }
                },
            );
        }
    }

    /// Fills `rect` with corners rounded by `radius`.
    pub fn fill_rounded_rect(&mut self, rect: Bounds, radius: f64, color: [u8; 3], opacity: f64) {
        let radius = radius
            .min((rect.max_x - rect.min_x) / 2.0)
            .min((rect.max_y - rect.min_y) / 2.0)
            .max(0.0);
        self.paint(
            rect,
            |_| Some(()),
            |(), (x, y)| {
                if x < rect.min_x || x > rect.max_x || y < rect.min_y || y > rect.max_y {
                    return false;
                }
                let nearest = (
                    x.clamp(rect.min_x + radius, rect.max_x - radius),
                    y.clamp(rect.min_y + radius, rect.max_y - radius),
                );
                within_circle((x, y), nearest, radius)
            },
            color,
            opacity,
        );
    }

    /// The image as straight (not premultiplied) RGBA bytes, rows top to bottom.
    pub fn into_rgba(self) -> Vec<u8> {
        let to_byte = |value: f64| (value * 255.0).round().clamp(0.0, 255.0) as u8;
        let mut bytes = Vec::with_capacity(self.pixels.len() * 4);
        for [r, g, b, a] in self.pixels {
            if a <= 1e-9 {
                bytes.extend_from_slice(&[0; 4]);
            } else {
                bytes.extend_from_slice(&[
                    to_byte(r / a),
                    to_byte(g / a),
                    to_byte(b / a),
                    to_byte(a),
                ]);
            }
        }
        bytes
    }
}

fn within_circle((x, y): Point, (cx, cy): Point, radius: f64) -> bool {
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= radius * radius
}

/// One `<path>` of the glyph.
#[derive(Debug, Clone, PartialEq)]
pub struct GlyphPath {
    pub shape: Shape,
    /// Filled (a `fill` other than `none`).
    pub fill: bool,
    /// Stroked this wide (a `stroke` other than `none`).
    pub stroke_width: Option<f64>,
}

/// The glyph's paths, in drawing order.
#[derive(Debug, Clone, PartialEq)]
pub struct Glyph {
    pub paths: Vec<GlyphPath>,
}

impl Glyph {
    /// The box around the drawn glyph, strokes included.
    pub fn bounds(&self) -> Option<Bounds> {
        let boxes = self.paths.iter().filter_map(|path| {
            let half = path.stroke_width.unwrap_or(0.0) / 2.0;
            path.shape.bounds().map(|b| b.grow(half))
        });
        Bounds::of(boxes.flat_map(|b| [(b.min_x, b.min_y), (b.max_x, b.max_y)]))
    }

    /// The glyph scaled by `scale`, then moved by (`dx`, `dy`); strokes scale too.
    pub fn transformed(&self, scale: f64, dx: f64, dy: f64) -> Glyph {
        Glyph {
            paths: self
                .paths
                .iter()
                .map(|path| GlyphPath {
                    shape: path.shape.transformed(scale, dx, dy),
                    fill: path.fill,
                    stroke_width: path.stroke_width.map(|width| width * scale),
                })
                .collect(),
        }
    }

    /// Draws every path in `color`.
    pub fn draw(&self, canvas: &mut Canvas, color: [u8; 3], opacity: f64) {
        for path in &self.paths {
            if path.fill {
                canvas.fill_shape(&path.shape, color, opacity);
            }
            if let Some(width) = path.stroke_width {
                canvas.stroke_shape(&path.shape, width, color, opacity);
            }
        }
    }
}

/// The value of `name="…"` in an element's text.
fn attribute<'a>(element: &'a str, name: &str) -> Option<&'a str> {
    let pattern = format!("{name}=\"");
    let mut from = 0;
    while let Some(found) = element[from..].find(&pattern) {
        let at = from + found;
        let preceded_by_space = element[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
        let value_at = at + pattern.len();
        if preceded_by_space {
            let end = element[value_at..].find('"')?;
            return Some(&element[value_at..value_at + end]);
        }
        from = value_at;
    }
    None
}

/// The `<path>` elements of an SVG document (attributes `d`, `fill`, `stroke`,
/// `stroke-width`).
pub fn parse_svg(svg: &str) -> Result<Glyph, PathError> {
    let mut paths = Vec::new();
    let mut from = 0;
    while let Some(found) = svg[from..].find("<path") {
        let begin = from + found;
        let end = svg[begin..].find('>').map_or(svg.len(), |end| begin + end);
        let element = &svg[begin..end];
        let d = attribute(element, "d").ok_or(PathError {
            at: begin,
            message: "a path without d",
        })?;
        let painted = |name| attribute(element, name).is_some_and(|value| value != "none");
        let stroke_width = if painted("stroke") {
            let width = attribute(element, "stroke-width").unwrap_or("1");
            Some(width.trim().parse().map_err(|_| PathError {
                at: begin,
                message: "bad stroke-width",
            })?)
        } else {
            None
        };
        paths.push(GlyphPath {
            shape: Shape::parse(d)?,
            fill: attribute(element, "fill").is_none_or(|value| value != "none"),
            stroke_width,
        });
        from = end;
    }
    Ok(Glyph { paths })
}

/// The glyph from [`SOURCE`], parsed once.
pub fn glyph() -> &'static Glyph {
    static GLYPH: OnceLock<Glyph> = OnceLock::new();
    GLYPH.get_or_init(|| parse_svg(SOURCE).expect("assets/glyph.svg is valid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn assert_bounds(bounds: Bounds, expected: [f64; 4]) {
        let actual = [bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y];
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 1e-6, "{actual:?} vs {expected:?}");
        }
    }

    #[test]
    fn the_svg_has_an_outlined_and_a_solid_bubble() {
        let glyph = parse_svg(SOURCE).unwrap();
        assert_eq!(glyph.paths.len(), 2);
        let (outline, solid) = (&glyph.paths[0], &glyph.paths[1]);
        assert!(!outline.fill);
        assert!(solid.fill);
        assert_eq!(outline.stroke_width, Some(2.2));
        assert_eq!(solid.stroke_width, Some(2.2));
        assert_eq!(outline.shape.subpaths[0].points[0], (3.0, 4.0));
        assert!(outline.shape.subpaths[0].closed);
        assert_bounds(outline.shape.bounds().unwrap(), [2.0, 4.0, 20.0, 19.0]);
        assert_bounds(solid.shape.bounds().unwrap(), [12.0, 13.0, 29.0, 27.0]);
        assert_eq!(super::glyph(), &glyph);
    }

    #[test]
    fn attributes_need_a_space_before_their_name() {
        let element = r#"<path id="x" d="M0 0" stroke-width="3""#;
        assert_eq!(attribute(element, "d"), Some("M0 0"));
        assert_eq!(attribute(element, "width"), None);
        assert_eq!(attribute(element, "stroke-width"), Some("3"));
    }

    #[test]
    fn relative_and_absolute_commands_agree() {
        let absolute = Shape::parse("M1 2 L4 2 H6 V5 L1 5 Z").unwrap();
        let relative = Shape::parse("m1,2 3 0h2v3l-5 0z").unwrap();
        assert_eq!(absolute, relative);
        assert_eq!(
            absolute.subpaths[0].points,
            [(1.0, 2.0), (4.0, 2.0), (6.0, 2.0), (6.0, 5.0), (1.0, 5.0)]
        );
    }

    #[test]
    fn numbers_may_run_together() {
        let shape = Shape::parse("M.5-2l1-1 -.5.25 1e1 0").unwrap();
        assert_eq!(
            shape.subpaths[0].points,
            [(0.5, -2.0), (1.5, -3.0), (1.0, -2.75), (11.0, -2.75)]
        );
        assert!(!shape.subpaths[0].closed);
    }

    #[test]
    fn a_path_continues_from_its_start_after_z() {
        let shape = Shape::parse("M1 1h2v2zl0 5").unwrap();
        assert_eq!(shape.subpaths.len(), 2);
        assert_eq!(shape.subpaths[1].points, [(1.0, 1.0), (1.0, 6.0)]);
    }

    #[test]
    fn bad_path_data_is_an_error() {
        assert!(Shape::parse("L1").is_err());
        assert!(Shape::parse("1 2").is_err());
        assert!(Shape::parse("M1 1 Q2 2 3 3").is_err());
        assert!(Shape::parse("M1 1z 2 2").is_err());
    }

    #[test]
    fn a_quarter_arc_is_flattened_along_the_circle() {
        let shape = Shape::parse("M18 4a2 2 0 0 1 2 2").unwrap();
        let points = &shape.subpaths[0].points;
        assert_eq!(points.first(), Some(&(18.0, 4.0)));
        assert_eq!(points.last(), Some(&(20.0, 6.0)));
        assert!(points.len() >= 9, "{}", points.len());
        for (x, y) in points {
            let radius = ((x - 18.0).powi(2) + (y - 6.0).powi(2)).sqrt();
            assert!(near(radius, 2.0), "{x},{y}");
        }
        assert!(
            points
                .iter()
                .any(|(x, y)| (x - 19.414).abs() < 0.01 && (y - 4.586).abs() < 0.01)
        );
    }

    #[test]
    fn small_arc_radii_are_scaled_up() {
        let shape = Shape::parse("M0 0A1 1 0 0 1 4 0").unwrap();
        let points = &shape.subpaths[0].points;
        assert_eq!(points.last(), Some(&(4.0, 0.0)));
        for (x, y) in points {
            assert!(near(((x - 2.0).powi(2) + y * y).sqrt(), 2.0), "{x},{y}");
        }
        // Sweep 1 from the left to the right goes through the top (y < 0).
        assert!(points.iter().any(|(_, y)| *y < -1.9));
    }

    fn alpha(canvas: Canvas, x: usize, y: usize) -> u8 {
        let size = canvas.size as usize;
        canvas.into_rgba()[(y * size + x) * 4 + 3]
    }

    #[test]
    fn fill_covers_the_inside_only() {
        let square = Shape::parse("M8 8h16v16h-16z").unwrap();
        let mut canvas = Canvas::new(32);
        canvas.fill_shape(&square, [10, 20, 30], 1.0);
        let pixels = canvas.clone().into_rgba();
        assert_eq!(&pixels[(16 * 32 + 16) * 4..][..4], &[10, 20, 30, 255]);
        assert_eq!(alpha(canvas.clone(), 4, 4), 0);
        assert_eq!(alpha(canvas, 24, 16), 0);
    }

    #[test]
    fn stroke_covers_the_outline_only() {
        let square = Shape::parse("M8 8h16v16h-16z").unwrap();
        let mut canvas = Canvas::new(32);
        canvas.stroke_shape(&square, 2.0, [255, 255, 255], 1.0);
        assert_eq!(alpha(canvas.clone(), 16, 16), 0);
        assert_eq!(alpha(canvas.clone(), 7, 16), 255);
        assert_eq!(alpha(canvas.clone(), 8, 16), 255);
        assert_eq!(alpha(canvas, 10, 16), 0);
    }

    #[test]
    fn half_covered_pixels_are_half_transparent_with_a_straight_colour() {
        let mut canvas = Canvas::new(32);
        canvas.fill_rounded_rect(
            Bounds {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 16.5,
                max_y: 32.0,
            },
            0.0,
            [200, 100, 50],
            1.0,
        );
        let pixels = canvas.into_rgba();
        assert_eq!(&pixels[(5 * 32 + 16) * 4..][..4], &[200, 100, 50, 128]);
    }

    #[test]
    fn clear_circle_knocks_out_and_fill_circle_composites() {
        let mut canvas = Canvas::new(32);
        canvas.fill_circle((16.0, 16.0), 10.0, [255, 0, 0], 1.0);
        canvas.clear_circle((16.0, 16.0), 4.0);
        canvas.fill_circle((16.0, 16.0), 2.0, [0, 0, 255], 0.5);
        let pixels = canvas.into_rgba();
        let at = |x: usize, y: usize| &pixels[(y * 32 + x) * 4..][..4];
        assert_eq!(at(16, 16), &[0, 0, 255, 128]);
        assert_eq!(at(16, 18)[3], 0);
        assert_eq!(at(16, 22), &[255, 0, 0, 255]);
        assert_eq!(at(0, 0)[3], 0);
    }

    #[test]
    fn an_empty_canvas_is_fine() {
        let mut canvas = Canvas::new(0);
        canvas.fill_circle((1.0, 1.0), 1.0, [0; 3], 1.0);
        assert!(canvas.into_rgba().is_empty());
    }
}
