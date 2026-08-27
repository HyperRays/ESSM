//! Canvas chart programs: sparklines, area charts, donut, histogram, and a
//! squarified treemap with hover and click-through to the explorer.

use iced::advanced::text::Alignment as TextAlignment;
use iced::alignment::Vertical;
use iced::mouse;
use iced::widget::canvas::{self, Event, Frame, Geometry, Path, Stroke};
use iced::{Color, Font, Pixels, Point, Rectangle, Renderer, Size, Theme};

use crate::app::THROUGHPUT_HISTORY;
use crate::format::human_bytes;
use crate::theme::ui;

fn label_text(content: String, position: Point, color: Color) -> canvas::Text {
    canvas::Text {
        content,
        position,
        color,
        size: Pixels(11.0),
        font: Font::MONOSPACE,
        ..canvas::Text::default()
    }
}

/// A tiny filled line for KPI cards. Newest sample first, drawn rightmost.
pub struct Sparkline {
    pub samples: Vec<u64>,
    pub color: Color,
}

impl<Message> canvas::Program<Message> for Sparkline {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());
        draw_series(
            &mut frame,
            &self.samples,
            THROUGHPUT_HISTORY,
            self.color,
            1.2,
            true,
        );
        vec![frame.into_geometry()]
    }
}

/// A full area chart with gridlines and value labels. Newest sample first.
pub struct AreaChart {
    pub samples: Vec<u64>,
    pub color: Color,
    pub format: fn(u64) -> String,
}

impl<Message> canvas::Program<Message> for AreaChart {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());
        let width = frame.width();
        let height = frame.height();

        // A finished-instantly scan may never have produced two samples;
        // empty axes read as a bug, so say what is happening instead.
        if self.samples.len() < 2 {
            frame.fill_text(canvas::Text {
                content:
                    "Not enough samples \u{2014} the scan completed within one sampling window"
                        .to_owned(),
                position: Point::new(width / 2.0, height / 2.0),
                color: ui().label,
                size: Pixels(12.0),
                align_x: TextAlignment::Center,
                align_y: Vertical::Center,
                ..canvas::Text::default()
            });
            return vec![frame.into_geometry()];
        }
        let peak = self.samples.iter().copied().max().unwrap_or(0).max(1);

        // Quarter gridlines with labels.
        for quarter in 1..=3_u64 {
            let y = height - (quarter as f32 / 4.0) * height / 1.05;
            frame.stroke(
                &Path::line(Point::new(0.0, y), Point::new(width, y)),
                Stroke::default()
                    .with_color(ui().panel_highlight)
                    .with_width(1.0),
            );
            frame.fill_text(label_text(
                (self.format)(peak * quarter / 4),
                Point::new(6.0, y - 14.0),
                ui().label,
            ));
        }
        frame.fill_text(label_text(
            format!("peak {}", (self.format)(peak)),
            Point::new(6.0, 4.0),
            ui().label,
        ));

        draw_series(
            &mut frame,
            &self.samples,
            THROUGHPUT_HISTORY,
            self.color,
            1.8,
            true,
        );
        vec![frame.into_geometry()]
    }
}

/// Shared polyline + translucent fill; x is right-anchored over `window`.
fn draw_series(
    frame: &mut Frame,
    samples: &[u64],
    window: usize,
    color: Color,
    stroke_width: f32,
    fill: bool,
) {
    if samples.len() < 2 {
        return;
    }
    let width = frame.width();
    let height = frame.height();
    let peak = samples.iter().copied().max().unwrap_or(0).max(1) as f32 * 1.05;
    let span = (window.saturating_sub(1)).max(1) as f32;
    let point = |age: usize, value: u64| {
        let x = width * (span - age as f32) / span;
        let y = height - (value as f32 / peak) * height;
        Point::new(x, y.clamp(1.0, height - 1.0))
    };

    if fill {
        let area = Path::new(|builder| {
            builder.move_to(Point::new(point(0, samples[0]).x, height));
            builder.line_to(point(0, samples[0]));
            for (age, &value) in samples.iter().enumerate().skip(1) {
                builder.line_to(point(age, value));
            }
            builder.line_to(Point::new(point(samples.len() - 1, 0).x, height));
            builder.close();
        });
        frame.fill(&area, Color { a: 0.14, ..color });
    }

    let line = Path::new(|builder| {
        builder.move_to(point(0, samples[0]));
        for (age, &value) in samples.iter().enumerate().skip(1) {
            builder.line_to(point(age, value));
        }
    });
    frame.stroke(
        &line,
        Stroke::default().with_color(color).with_width(stroke_width),
    );
}

/// A filled annular sector sampled finely enough that chord error stays
/// far below a pixel. Stroked `Path::arc`s are avoided on purpose: iced
/// approximates arcs with béziers and wide strokes flatten them into
/// bar-like shapes, so ring charts build their slices as exact filled
/// polygons instead.
fn annular_sector(center: Point, inner: f32, outer: f32, start: f32, sweep: f32) -> Path {
    let steps = ((sweep / 0.03).abs().ceil() as usize).clamp(2, 512);
    Path::new(|builder| {
        for step in 0..=steps {
            let angle = start + sweep * (step as f32 / steps as f32);
            let point = Point::new(
                center.x + angle.cos() * outer,
                center.y + angle.sin() * outer,
            );
            if step == 0 {
                builder.move_to(point);
            } else {
                builder.line_to(point);
            }
        }
        for step in (0..=steps).rev() {
            let angle = start + sweep * (step as f32 / steps as f32);
            builder.line_to(Point::new(
                center.x + angle.cos() * inner,
                center.y + angle.sin() * inner,
            ));
        }
        builder.close();
    })
}

/// A donut of labeled slices with a center caption.
pub struct Donut {
    pub slices: Vec<(u64, Color)>,
    pub center_title: String,
    pub center_value: String,
}

impl<Message> canvas::Program<Message> for Donut {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());
        let center = frame.center();
        let thickness = 22.0_f32;
        let radius = (frame.width().min(frame.height()) / 2.0 - thickness / 2.0 - 2.0).max(10.0);
        let total: u64 = self.slices.iter().map(|(value, _)| value).sum();

        if total > 0 {
            let gap = 0.02_f32; // radians between slices
            let mut angle = -std::f32::consts::FRAC_PI_2;
            for &(value, color) in &self.slices {
                if value == 0 {
                    continue;
                }
                let sweep = (value as f32 / total as f32) * std::f32::consts::TAU;
                let drawn = (sweep - gap).max(0.005);
                frame.fill(
                    &annular_sector(
                        center,
                        radius - thickness / 2.0,
                        radius + thickness / 2.0,
                        angle,
                        drawn,
                    ),
                    color,
                );
                angle += sweep;
            }
        }

        frame.fill_text(canvas::Text {
            content: self.center_value.clone(),
            position: Point::new(center.x, center.y - 10.0),
            color: ui().text,
            size: Pixels(18.0),
            align_x: TextAlignment::Center,
            align_y: Vertical::Center,
            ..canvas::Text::default()
        });
        frame.fill_text(canvas::Text {
            content: self.center_title.clone(),
            position: Point::new(center.x, center.y + 10.0),
            color: ui().label,
            size: Pixels(12.0),
            align_x: TextAlignment::Center,
            align_y: Vertical::Center,
            ..canvas::Text::default()
        });
        vec![frame.into_geometry()]
    }
}

/// File-size distribution over log₂ buckets, square-root count scale.
pub struct Histogram {
    pub counts: Vec<u64>,
    pub color: Color,
}

impl<Message> canvas::Program<Message> for Histogram {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());
        let width = frame.width();
        let height = frame.height();
        let label_area = 16.0_f32;
        let chart_height = (height - label_area).max(10.0);

        let first = self.counts.iter().position(|&count| count > 0).unwrap_or(0);
        let last = self
            .counts
            .iter()
            .rposition(|&count| count > 0)
            .unwrap_or(0)
            .max(first + 7);
        let last = last.min(self.counts.len().saturating_sub(1));
        let buckets = &self.counts[first..=last];
        let peak = (buckets.iter().copied().max().unwrap_or(0).max(1) as f32).sqrt();

        let slot = width / buckets.len() as f32;
        for (index, &count) in buckets.iter().enumerate() {
            if count == 0 {
                continue;
            }
            let bar_height = ((count as f32).sqrt() / peak) * (chart_height - 14.0);
            let x = index as f32 * slot;
            frame.fill_rectangle(
                Point::new(x + 1.0, chart_height - bar_height),
                Size::new((slot - 2.0).max(1.0), bar_height.max(1.0)),
                self.color,
            );
        }

        // Byte labels under the buckets that mark 1 KiB, 1 MiB, 1 GiB.
        for (bucket, label) in [(11_usize, "1K"), (21, "1M"), (31, "1G")] {
            if bucket >= first && bucket <= last {
                let x = (bucket - first) as f32 * slot;
                frame.fill_text(label_text(
                    label.to_owned(),
                    Point::new(x, chart_height + 2.0),
                    ui().label,
                ));
                frame.stroke(
                    &Path::line(
                        Point::new(x, chart_height),
                        Point::new(x, chart_height - 4.0),
                    ),
                    Stroke::default().with_color(ui().label).with_width(1.0),
                );
            }
        }
        vec![frame.into_geometry()]
    }
}

/// Distinguishes a double click from two selections, per hit target.
#[derive(Default)]
pub struct ClickTracker {
    last: Option<(std::time::Instant, usize)>,
}

impl ClickTracker {
    /// Records a click on `index`; true when it completes a double click.
    pub fn click(&mut self, index: usize) -> bool {
        let now = std::time::Instant::now();
        let double = self
            .last
            .is_some_and(|(at, last)| last == index && now.duration_since(at).as_millis() < 400);
        self.last = if double { None } else { Some((now, index)) };
        double
    }
}

/// Linear blend between two colors.
pub fn mix(from: Color, to: Color, amount: f32) -> Color {
    let t = amount.clamp(0.0, 1.0);
    Color {
        r: from.r + (to.r - from.r) * t,
        g: from.g + (to.g - from.g) * t,
        b: from.b + (to.b - from.b) * t,
        a: 1.0,
    }
}

/// One tile of the treemap.
#[derive(Clone, Debug)]
pub struct TreemapCell {
    pub directory_id: u32,
    pub label: String,
    pub bytes: u64,
    /// Whether double-clicking re-centers on the directory.
    pub drill: bool,
    /// Muted cells represent the parent's own files, not a directory.
    pub muted: bool,
}

/// Squarified treemap of the largest directories; hovering highlights and
/// clicking opens the directory in the explorer.
pub struct Treemap {
    pub cells: Vec<TreemapCell>,
    pub palette: Vec<Color>,
}

#[derive(Default)]
pub struct TreemapState {
    hovered: Option<usize>,
    clicks: ClickTracker,
}

impl canvas::Program<crate::Message> for Treemap {
    type State = TreemapState;

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<crate::Message>> {
        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let hovered = cursor.position_in(bounds).and_then(|position| {
                    let layout = squarify(&areas(&self.cells), bounds.size());
                    layout.iter().position(|cell| cell.contains(position))
                });
                if hovered != state.hovered {
                    state.hovered = hovered;
                    return Some(canvas::Action::request_redraw());
                }
                None
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let index = state.hovered?;
                let cell = self.cells.get(index)?;
                let message = if state.clicks.click(index) && cell.drill {
                    crate::Message::FocusDirectory(cell.directory_id)
                } else {
                    crate::Message::NodeSelected(cell.directory_id)
                };
                Some(canvas::Action::publish(message).and_capture())
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());
        let layout = squarify(&areas(&self.cells), bounds.size());

        for (index, rectangle) in layout.iter().enumerate() {
            let cell = &self.cells[index];
            let base = if cell.muted {
                mix(ui().label, ui().panel, 0.45)
            } else {
                self.palette[index % self.palette.len()]
            };
            let hovered = state.hovered == Some(index);
            let factor = if hovered { 0.78 } else { 0.94 };
            let fill = Color::from_rgb(base.r * factor, base.g * factor, base.b * factor);
            let label_color = crate::theme::on_color(fill);

            frame.fill_rectangle(
                Point::new(rectangle.x + 1.0, rectangle.y + 1.0),
                Size::new(
                    (rectangle.width - 2.0).max(0.5),
                    (rectangle.height - 2.0).max(0.5),
                ),
                fill,
            );

            if rectangle.width > 70.0 && rectangle.height > 30.0 {
                let characters = (rectangle.width / 7.5) as usize;
                let mut label = cell.label.clone();
                if label.chars().count() > characters {
                    label = label.chars().take(characters.saturating_sub(1)).collect();
                    label.push('…');
                }
                frame.fill_text(label_text(
                    label,
                    Point::new(rectangle.x + 6.0, rectangle.y + 5.0),
                    label_color,
                ));
                frame.fill_text(label_text(
                    human_bytes(cell.bytes),
                    Point::new(rectangle.x + 6.0, rectangle.y + 18.0),
                    Color {
                        a: 0.8,
                        ..label_color
                    },
                ));
            }
        }

        if let Some(index) = state.hovered
            && let Some(cell) = self.cells.get(index)
        {
            frame.fill_text(label_text(
                format!("{} · {}", cell.label, human_bytes(cell.bytes)),
                Point::new(8.0, frame.height() - 18.0),
                ui().text,
            ));
        }
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.hovered.is_some() {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}

fn areas(cells: &[TreemapCell]) -> Vec<f64> {
    cells.iter().map(|cell| cell.bytes.max(1) as f64).collect()
}

/// Squarified treemap layout (Bruls, Huizing, van Wijk). `values` must be
/// sorted descending; returns one rectangle per value, filling `size`.
pub fn squarify(values: &[f64], size: Size) -> Vec<Rectangle> {
    let total: f64 = values.iter().sum();
    if total <= 0.0 || values.is_empty() || size.width <= 0.0 || size.height <= 0.0 {
        return Vec::new();
    }
    let scale = f64::from(size.width) * f64::from(size.height) / total;
    let mut areas: Vec<f64> = values.iter().map(|value| value * scale).collect();
    let mut rectangles = Vec::with_capacity(values.len());
    let mut free = Rectangle {
        x: 0.0,
        y: 0.0,
        width: size.width,
        height: size.height,
    };

    let mut start = 0;
    while start < areas.len() {
        let side = f64::from(free.width.min(free.height)).max(1.0);
        let mut end = start + 1;
        let mut row_sum: f64 = areas[start];
        let mut worst = worst_aspect(&areas[start..end], row_sum, side);
        while end < areas.len() {
            let candidate_sum = row_sum + areas[end];
            let candidate_worst = worst_aspect(&areas[start..=end], candidate_sum, side);
            if candidate_worst > worst {
                break;
            }
            row_sum = candidate_sum;
            worst = candidate_worst;
            end += 1;
        }

        // Fix the row as a strip spanning the shorter side of the free
        // rectangle; its thickness extends along the longer side.
        let vertical_strip = free.width >= free.height; // side == height
        let thickness = if vertical_strip {
            ((row_sum / side) as f32).min(free.width)
        } else {
            ((row_sum / side) as f32).min(free.height)
        };
        let mut offset = 0.0_f32;
        for area in &areas[start..end] {
            let length = ((*area / row_sum * side) as f32).min(side as f32 - offset);
            let rectangle = if vertical_strip {
                Rectangle {
                    x: free.x,
                    y: free.y + offset,
                    width: thickness,
                    height: length,
                }
            } else {
                Rectangle {
                    x: free.x + offset,
                    y: free.y,
                    width: length,
                    height: thickness,
                }
            };
            rectangles.push(rectangle);
            offset += length;
        }
        if vertical_strip {
            free.x += thickness;
            free.width = (free.width - thickness).max(0.0);
        } else {
            free.y += thickness;
            free.height = (free.height - thickness).max(0.0);
        }
        // Guard against degenerate zero-thickness rows.
        if thickness <= f32::EPSILON {
            for area in areas[end..].iter_mut() {
                *area = 0.0;
            }
        }
        start = end;
    }
    rectangles
}

fn worst_aspect(row: &[f64], sum: f64, side: f64) -> f64 {
    let side_squared = side * side;
    let sum_squared = sum * sum;
    row.iter().fold(0.0_f64, |worst, &area| {
        if area <= 0.0 {
            return worst;
        }
        let ratio = (side_squared * area / sum_squared).max(sum_squared / (side_squared * area));
        worst.max(ratio)
    })
}

// ------------------------------------------------------------- sunburst --

/// One arc of the sunburst. Angles are radians clockwise from the
/// positive x-axis, matching the canvas arc convention.
#[derive(Clone, Debug)]
pub struct SunburstSlice {
    /// `None` marks a non-interactive filler for aggregated small slices.
    pub directory_id: Option<u32>,
    /// 1-based ring index, innermost first.
    pub ring: u8,
    pub start: f32,
    pub sweep: f32,
    pub color: Color,
    pub drill: bool,
    pub label: String,
    pub bytes: u64,
}

/// Hierarchical pie of the focused subtree: rings are depth levels and
/// angular spans are recursive bytes. Click selects, double-click drills,
/// the center ring steps back up.
pub struct Sunburst {
    pub slices: Vec<SunburstSlice>,
    pub rings: u8,
    pub center_label: String,
    pub center_bytes: u64,
    /// Focus parent; clicking the center hole navigates here.
    pub up: Option<u32>,
}

#[derive(Default)]
pub struct SunburstState {
    hovered: Option<SunburstHit>,
    clicks: ClickTracker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SunburstHit {
    Center,
    Slice(usize),
}

impl Sunburst {
    fn geometry(&self, bounds: Rectangle) -> (Point, f32, f32) {
        let center = Point::new(bounds.width / 2.0, bounds.height / 2.0);
        let radius = (bounds.width.min(bounds.height) / 2.0 - 8.0).max(40.0);
        let hole = radius * 0.28;
        let thickness = (radius - hole) / f32::from(self.rings.max(1));
        (center, hole, thickness)
    }

    fn hit(&self, bounds: Rectangle, position: Point) -> Option<SunburstHit> {
        let (center, hole, thickness) = self.geometry(bounds);
        let dx = position.x - bounds.x - center.x;
        let dy = position.y - bounds.y - center.y;
        let distance = (dx * dx + dy * dy).sqrt();
        if distance < hole {
            return Some(SunburstHit::Center);
        }
        let ring = ((distance - hole) / thickness).floor() as i64 + 1;
        if ring < 1 || ring > i64::from(self.rings) {
            return None;
        }
        let mut angle = dy.atan2(dx);
        if angle < -std::f32::consts::FRAC_PI_2 {
            angle += std::f32::consts::TAU;
        }
        self.slices
            .iter()
            .position(|slice| {
                i64::from(slice.ring) == ring
                    && angle >= slice.start
                    && angle < slice.start + slice.sweep
            })
            .map(SunburstHit::Slice)
    }
}

impl canvas::Program<crate::Message> for Sunburst {
    type State = SunburstState;

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<crate::Message>> {
        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let hovered = cursor.position_in(bounds).and_then(|position| {
                    self.hit(
                        bounds,
                        Point::new(position.x + bounds.x, position.y + bounds.y),
                    )
                });
                if hovered != state.hovered {
                    state.hovered = hovered;
                    return Some(canvas::Action::request_redraw());
                }
                None
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                match state.hovered? {
                    SunburstHit::Center => {
                        let up = self.up?;
                        Some(
                            canvas::Action::publish(crate::Message::FocusDirectory(up))
                                .and_capture(),
                        )
                    }
                    SunburstHit::Slice(index) => {
                        let slice = self.slices.get(index)?;
                        let id = slice.directory_id?;
                        let message = if state.clicks.click(index) && slice.drill {
                            crate::Message::FocusDirectory(id)
                        } else {
                            crate::Message::NodeSelected(id)
                        };
                        Some(canvas::Action::publish(message).and_capture())
                    }
                }
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());
        let local = Rectangle {
            x: 0.0,
            y: 0.0,
            width: bounds.width,
            height: bounds.height,
        };
        let (center, hole, thickness) = self.geometry(local);

        for (index, slice) in self.slices.iter().enumerate() {
            let radius = hole + thickness * (f32::from(slice.ring) - 0.5);
            let hovered = state.hovered == Some(SunburstHit::Slice(index));
            let color = if hovered {
                mix(slice.color, Color::BLACK, 0.18)
            } else {
                slice.color
            };
            let inner = hole + thickness * (f32::from(slice.ring) - 1.0) + 1.0;
            // A filler has nothing deeper inside it, so it extends to the
            // rim; a white notch above it would read as a broken ring.
            let outer_ring = if slice.directory_id.is_none() {
                self.rings
            } else {
                slice.ring
            };
            let outer = hole + thickness * f32::from(outer_ring) - 1.0;
            // Constant-width separators: shrinking the angular gap with
            // radius keeps neighbors parted by the same ~2px on every
            // ring, matching the radial gap between rings.
            let gap = 2.0 / radius;
            let (start, sweep) = if slice.sweep > 2.0 * gap {
                (slice.start + gap / 2.0, slice.sweep - gap)
            } else {
                (slice.start, slice.sweep)
            };
            frame.fill(&annular_sector(center, inner, outer, start, sweep), color);

            // Label the slice when its arc is long enough to carry text.
            // The baseline runs tangentially so text follows its band
            // instead of spilling across rings, flipped on the left half
            // to stay upright; the length cap bounds how far a straight
            // baseline can drift from the curved band.
            let arc = slice.sweep * radius;
            if slice.directory_id.is_some() && arc > 46.0 {
                let budget = ((arc - 12.0).min(150.0) / 6.8) as usize;
                let mid = slice.start + slice.sweep / 2.0;
                let mut rotation = mid + std::f32::consts::FRAC_PI_2;
                let upright = rotation.rem_euclid(std::f32::consts::TAU);
                if (std::f32::consts::FRAC_PI_2..3.0 * std::f32::consts::FRAC_PI_2)
                    .contains(&upright)
                {
                    rotation += std::f32::consts::PI;
                }
                frame.with_save(|frame| {
                    frame.translate(iced::Vector::new(
                        center.x + mid.cos() * radius,
                        center.y + mid.sin() * radius,
                    ));
                    frame.rotate(rotation);
                    frame.fill_text(canvas::Text {
                        content: fit_label(&slice.label, budget),
                        position: Point::ORIGIN,
                        color: on_slice(color),
                        size: Pixels(11.0),
                        font: Font::MONOSPACE,
                        align_x: TextAlignment::Center,
                        align_y: Vertical::Center,
                        ..canvas::Text::default()
                    });
                });
            }
        }

        // Center: focus name + size; doubles as the "go up" control.
        let center_hovered = state.hovered == Some(SunburstHit::Center);
        if center_hovered && self.up.is_some() {
            frame.fill(&Path::circle(center, hole - 4.0), ui().panel_highlight);
        }
        frame.fill_text(canvas::Text {
            content: fit_label(&self.center_label, (hole / 3.8) as usize),
            position: Point::new(center.x, center.y - 10.0),
            color: ui().text,
            size: Pixels(13.0),
            font: Font::MONOSPACE,
            align_x: TextAlignment::Center,
            align_y: Vertical::Center,
            ..canvas::Text::default()
        });
        frame.fill_text(canvas::Text {
            content: human_bytes(self.center_bytes),
            position: Point::new(center.x, center.y + 8.0),
            color: ui().label,
            size: Pixels(12.0),
            align_x: TextAlignment::Center,
            align_y: Vertical::Center,
            ..canvas::Text::default()
        });
        if self.up.is_some() {
            frame.fill_text(canvas::Text {
                content: "\u{2191} up".to_owned(),
                position: Point::new(center.x, center.y + 24.0),
                color: ui().label,
                size: Pixels(10.0),
                align_x: TextAlignment::Center,
                align_y: Vertical::Center,
                ..canvas::Text::default()
            });
        }

        if let Some(SunburstHit::Slice(index)) = state.hovered
            && let Some(slice) = self.slices.get(index)
            && slice.directory_id.is_some()
        {
            frame.fill_text(label_text(
                format!("{} \u{b7} {}", slice.label, human_bytes(slice.bytes)),
                Point::new(8.0, frame.height() - 18.0),
                ui().text,
            ));
        }
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        match state.hovered {
            Some(SunburstHit::Slice(_)) => mouse::Interaction::Pointer,
            Some(SunburstHit::Center) if self.up.is_some() => mouse::Interaction::Pointer,
            _ => mouse::Interaction::default(),
        }
    }
}

fn fit_label(label: &str, budget: usize) -> String {
    if label.chars().count() <= budget.max(1) {
        return label.to_owned();
    }
    let mut fitted: String = label.chars().take(budget.max(2) - 1).collect();
    fitted.push('\u{2026}');
    fitted
}

fn on_slice(color: Color) -> Color {
    crate::theme::on_color(color)
}

// ----------------------------------------------------------- node graph --

/// Which side of a node its label sits on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LabelSide {
    Left,
    Right,
}

#[derive(Clone, Debug)]
pub struct GraphLabel {
    pub name: String,
    /// Quiet suffix such as "1.7 GiB \u{b7} 12 dirs".
    pub detail: String,
    pub side: LabelSide,
}

/// One node of the outline graph. Coordinates are layout pixels at zoom 1.
#[derive(Clone, Debug)]
pub struct GraphNode {
    pub directory_id: u32,
    pub x: f32,
    pub y: f32,
    pub radius: f32,
    pub color: Color,
    /// 0..=1; ancestors fade out.
    pub alpha: f32,
    pub label: Option<GraphLabel>,
    pub bytes: u64,
    /// Whether clicking this node re-centers the ladder on it.
    pub navigates: bool,
}

/// An orthogonal guide edge, drawn as a polyline.
#[derive(Clone, Debug)]
pub struct GraphEdge {
    pub points: Vec<(f32, f32)>,
}

/// An explorable outline of the tree, laid out so that the two visual
/// invariants hold by construction: every node owns a full row, so labels
/// cannot overlap; and edges are orthogonal guides that run left of the
/// nodes while labels sit to the right, so edges cross neither labels nor
/// each other. Clicking a node re-centers on it; dragging pans and
/// scrolling zooms.
pub struct NodeGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Children beyond the display cap, mentioned in a corner caption.
    pub hidden_children: usize,
    /// Centered directory; a change resets pan and zoom.
    pub focus: u32,
}

pub struct NodeGraphState {
    hovered: Option<usize>,
    offset: iced::Vector,
    zoom: f32,
    panning: Option<Point>,
    last_focus: Option<u32>,
}

impl Default for NodeGraphState {
    fn default() -> Self {
        Self {
            hovered: None,
            offset: iced::Vector::new(0.0, 0.0),
            zoom: 1.0,
            panning: None,
            last_focus: None,
        }
    }
}

impl NodeGraph {
    fn node_position(state: &NodeGraphState, bounds_size: Size, node: &GraphNode) -> Point {
        let center = Point::new(bounds_size.width / 2.0, bounds_size.height / 2.0);
        Point::new(
            center.x + node.x * state.zoom + state.offset.x,
            center.y + node.y * state.zoom + state.offset.y,
        )
    }

    fn point_position(state: &NodeGraphState, bounds_size: Size, point: (f32, f32)) -> Point {
        let center = Point::new(bounds_size.width / 2.0, bounds_size.height / 2.0);
        Point::new(
            center.x + point.0 * state.zoom + state.offset.x,
            center.y + point.1 * state.zoom + state.offset.y,
        )
    }

    fn hit(&self, state: &NodeGraphState, bounds_size: Size, position: Point) -> Option<usize> {
        self.nodes.iter().position(|node| {
            let at = Self::node_position(state, bounds_size, node);
            let dx = position.x - at.x;
            let dy = position.y - at.y;
            (dx * dx + dy * dy).sqrt() <= (node.radius * state.zoom).max(9.0) + 3.0
        })
    }
}

impl canvas::Program<crate::Message> for NodeGraph {
    type State = NodeGraphState;

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<crate::Message>> {
        // Re-centering resets the viewport.
        if state.last_focus != Some(self.focus) {
            state.last_focus = Some(self.focus);
            state.offset = iced::Vector::new(0.0, 0.0);
            state.zoom = 1.0;
            state.panning = None;
        }

        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let position = cursor.position_in(bounds)?;
                if let Some(previous) = state.panning {
                    state.offset +=
                        iced::Vector::new(position.x - previous.x, position.y - previous.y);
                    state.panning = Some(position);
                    return Some(canvas::Action::request_redraw().and_capture());
                }
                let hovered = self.hit(state, bounds.size(), position);
                if hovered != state.hovered {
                    state.hovered = hovered;
                    return Some(canvas::Action::request_redraw());
                }
                None
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                cursor.position_in(bounds)?;
                let lines = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y,
                    mouse::ScrollDelta::Pixels { y, .. } => *y / 40.0,
                };
                state.zoom = (state.zoom * (1.0 + lines * 0.12)).clamp(0.4, 3.0);
                Some(canvas::Action::request_redraw().and_capture())
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let position = cursor.position_in(bounds)?;
                match self.hit(state, bounds.size(), position) {
                    Some(index) => {
                        let node = self.nodes.get(index)?;
                        let message = if node.navigates && node.directory_id != self.focus {
                            crate::Message::FocusDirectory(node.directory_id)
                        } else {
                            crate::Message::NodeSelected(node.directory_id)
                        };
                        Some(canvas::Action::publish(message).and_capture())
                    }
                    None => {
                        state.panning = Some(position);
                        Some(canvas::Action::request_redraw().and_capture())
                    }
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.panning = None;
                None
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());
        let guide = mix(ui().label, ui().panel, 0.55);

        for edge in &self.edges {
            if edge.points.len() < 2 {
                continue;
            }
            let path = Path::new(|builder| {
                builder.move_to(Self::point_position(state, bounds.size(), edge.points[0]));
                for &point in &edge.points[1..] {
                    builder.line_to(Self::point_position(state, bounds.size(), point));
                }
            });
            frame.stroke(&path, Stroke::default().with_color(guide).with_width(1.0));
        }

        for (index, node) in self.nodes.iter().enumerate() {
            let at = Self::node_position(state, bounds.size(), node);
            let radius = node.radius * state.zoom;
            let hovered = state.hovered == Some(index);
            let base = if hovered {
                mix(node.color, Color::BLACK, 0.18)
            } else {
                node.color
            };
            frame.fill(
                &Path::circle(at, radius),
                Color {
                    a: node.alpha,
                    ..base
                },
            );
            if hovered {
                frame.stroke(
                    &Path::circle(at, radius + 2.0),
                    Stroke::default().with_color(ui().text).with_width(1.0),
                );
            }

            let Some(label) = &node.label else { continue };
            let text_alpha = node.alpha.max(0.6);
            match label.side {
                LabelSide::Right => {
                    let name_x = at.x + radius + 8.0;
                    frame.fill_text(canvas::Text {
                        content: label.name.clone(),
                        position: Point::new(name_x, at.y - 7.0),
                        color: Color {
                            a: text_alpha,
                            ..ui().text
                        },
                        size: Pixels(12.0),
                        font: Font::MONOSPACE,
                        ..canvas::Text::default()
                    });
                    if !label.detail.is_empty() {
                        frame.fill_text(canvas::Text {
                            content: label.detail.clone(),
                            position: Point::new(
                                name_x + label.name.chars().count() as f32 * 7.3 + 10.0,
                                at.y - 6.0,
                            ),
                            color: Color {
                                a: text_alpha,
                                ..ui().label
                            },
                            size: Pixels(11.0),
                            ..canvas::Text::default()
                        });
                    }
                }
                LabelSide::Left => {
                    frame.fill_text(canvas::Text {
                        content: label.name.clone(),
                        position: Point::new(at.x - radius - 8.0, at.y - 7.0),
                        color: Color {
                            a: text_alpha,
                            ..ui().text
                        },
                        size: Pixels(12.0),
                        font: Font::MONOSPACE,
                        align_x: TextAlignment::Right,
                        ..canvas::Text::default()
                    });
                    if !label.detail.is_empty() {
                        frame.fill_text(canvas::Text {
                            content: label.detail.clone(),
                            position: Point::new(at.x - radius - 8.0, at.y + 7.0),
                            color: Color {
                                a: text_alpha,
                                ..ui().label
                            },
                            size: Pixels(10.0),
                            align_x: TextAlignment::Right,
                            ..canvas::Text::default()
                        });
                    }
                }
            }
        }

        if let Some(index) = state.hovered
            && let Some(node) = self.nodes.get(index)
        {
            let name = node
                .label
                .as_ref()
                .map(|label| label.name.clone())
                .unwrap_or_default();
            frame.fill_text(label_text(
                format!("{name} \u{b7} {}", human_bytes(node.bytes)),
                Point::new(8.0, frame.height() - 18.0),
                ui().text,
            ));
        }
        if self.hidden_children > 0 {
            frame.fill_text(canvas::Text {
                content: format!("\u{2026} +{} smaller directories", self.hidden_children),
                position: Point::new(frame.width() - 8.0, frame.height() - 18.0),
                color: ui().label,
                size: Pixels(11.0),
                align_x: TextAlignment::Right,
                ..canvas::Text::default()
            });
        }
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.panning.is_some() {
            mouse::Interaction::Grabbing
        } else if state.hovered.is_some() {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squarified_layout_fills_the_rectangle() {
        let values = vec![6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let size = Size::new(600.0, 400.0);
        let layout = squarify(&values, size);

        assert_eq!(layout.len(), values.len());
        let total_area: f32 = layout
            .iter()
            .map(|rectangle| rectangle.width * rectangle.height)
            .sum();
        assert!((total_area - 600.0 * 400.0).abs() < 600.0 * 400.0 * 0.01);
        for rectangle in &layout {
            assert!(rectangle.x >= -0.5 && rectangle.y >= -0.5);
            assert!(rectangle.x + rectangle.width <= size.width + 0.5);
            assert!(rectangle.y + rectangle.height <= size.height + 0.5);
        }
        // Larger values get larger tiles.
        let first = layout[0].width * layout[0].height;
        let last = layout[6].width * layout[6].height;
        assert!(first > last);
    }

    #[test]
    fn squarify_handles_degenerate_inputs() {
        assert!(squarify(&[], Size::new(100.0, 100.0)).is_empty());
        assert!(squarify(&[0.0, 0.0], Size::new(100.0, 100.0)).is_empty());
        assert_eq!(squarify(&[5.0], Size::new(100.0, 50.0)).len(), 1);
    }
}
