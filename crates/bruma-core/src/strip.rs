//! The strip: the one-row toolbar docked to the selection.

use crate::geom::{Point, Rect};
use crate::session::Tool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripItem {
    Tool(Tool),
    /// The current ink; clicking cycles to the next one.
    Color,
    Undo,
    Redo,
    Copy,
    /// Reads the text in the selection onto the clipboard.
    CopyText,
    Save,
    Pin,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StripLayout {
    pub rect: Rect,
    pub cells: Vec<(StripItem, Rect)>,
    /// X positions of the vertical separator hairlines.
    pub separators: Vec<f32>,
}

// Logical metrics, multiplied by the scale factor.
const CELL_W: f32 = 28.0;
const HEIGHT: f32 = 44.0;
const PAD: f32 = 4.0;
const SEP: f32 = 9.0;
const GAP: f32 = 8.0;

/// Places the strip for `selection` inside `bounds` (the monitor holding it).
///
/// Preferred spot is just below the selection, right-aligned. With no room
/// below it goes above; with no room either way it sits inside the selection.
pub fn layout(selection: Rect, bounds: Rect, scale: f32) -> StripLayout {
    let groups: [&[StripItem]; 4] = [
        &Tool::ALL.map(StripItem::Tool),
        &[StripItem::Color],
        &[StripItem::Undo, StripItem::Redo],
        &[
            StripItem::Copy,
            StripItem::CopyText,
            StripItem::Save,
            StripItem::Pin,
        ],
    ];
    let count: usize = groups.iter().map(|g| g.len()).sum();
    let cell_w = (CELL_W * scale).round();
    let height = (HEIGHT * scale).round();
    let pad = (PAD * scale).round();
    let sep = (SEP * scale).round();
    let gap = (GAP * scale).round();
    let width = pad * 2.0 + cell_w * count as f32 + sep * (groups.len() - 1) as f32;

    let mut x = selection.right() - width;
    let mut y = selection.bottom() + gap;
    if y + height > bounds.bottom() {
        y = selection.y - gap - height;
        if y < bounds.y {
            y = selection.bottom() - gap - height;
            x = selection.right() - gap - width;
        }
    }
    let x = x
        .clamp(bounds.x, (bounds.right() - width).max(bounds.x))
        .round();
    let y = y
        .clamp(bounds.y, (bounds.bottom() - height).max(bounds.y))
        .round();

    let mut cells = Vec::with_capacity(count);
    let mut separators = Vec::new();
    let mut cx = x + pad;
    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            separators.push(cx + (sep / 2.0).floor());
            cx += sep;
        }
        for item in group.iter() {
            cells.push((*item, Rect::new(cx, y, cell_w, height)));
            cx += cell_w;
        }
    }
    StripLayout {
        rect: Rect::new(x, y, width, height),
        cells,
        separators,
    }
}

impl StripItem {
    /// What the item is called, for its tooltip.
    pub fn name(self) -> &'static str {
        match self {
            StripItem::Tool(Tool::Select) => "Select and move",
            StripItem::Tool(Tool::Arrow) => "Arrow",
            StripItem::Tool(Tool::Line) => "Line",
            StripItem::Tool(Tool::Rect) => "Rectangle",
            StripItem::Tool(Tool::Ellipse) => "Ellipse",
            StripItem::Tool(Tool::Pen) => "Pen",
            StripItem::Tool(Tool::Marker) => "Highlighter",
            StripItem::Tool(Tool::Text) => "Text",
            StripItem::Tool(Tool::Step) => "Numbered step",
            StripItem::Tool(Tool::Blur) => "Blur",
            StripItem::Tool(Tool::Focus) => "Focus: keep this sharp",
            StripItem::Tool(Tool::Measure) => "Measure",
            StripItem::Color => "Ink (click for the next)",
            StripItem::Undo => "Undo",
            StripItem::Redo => "Redo",
            StripItem::Copy => "Copy and close",
            StripItem::CopyText => "Copy the text and close",
            StripItem::Save => "Save and close",
            StripItem::Pin => "Pin on screen",
        }
    }
}

impl StripLayout {
    pub fn hit(&self, p: Point) -> Option<StripItem> {
        self.cells
            .iter()
            .find(|(_, r)| r.contains(p))
            .map(|(item, _)| *item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect::new(0.0, 0.0, 1920.0, 1080.0);

    #[test]
    fn sits_below_selection_right_aligned() {
        let sel = Rect::new(600.0, 200.0, 800.0, 400.0);
        let s = layout(sel, SCREEN, 1.0);
        assert_eq!(s.rect.y, 608.0);
        assert_eq!(s.rect.right(), sel.right());
    }

    #[test]
    fn flips_above_when_no_room_below() {
        let sel = Rect::new(600.0, 500.0, 800.0, 570.0);
        let s = layout(sel, SCREEN, 1.0);
        assert_eq!(s.rect.bottom(), sel.y - 8.0);
    }

    #[test]
    fn moves_inside_for_full_screen_selection() {
        let s = layout(SCREEN, SCREEN, 1.0);
        assert!(s.rect.bottom() <= SCREEN.bottom() && s.rect.right() <= SCREEN.right());
        assert!(SCREEN.contains(Point::new(s.rect.x, s.rect.y)));
    }

    #[test]
    fn never_leaves_the_bounds() {
        for sel in [
            Rect::new(0.0, 0.0, 20.0, 20.0),
            Rect::new(1900.0, 1060.0, 20.0, 20.0),
            Rect::new(0.0, 1000.0, 40.0, 80.0),
        ] {
            for scale in [1.0, 1.5, 2.0] {
                let s = layout(sel, SCREEN, scale);
                assert!(s.rect.x >= 0.0 && s.rect.y >= 0.0, "{sel:?} @ {scale}");
                assert!(
                    s.rect.right() <= 1920.0 && s.rect.bottom() <= 1080.0,
                    "{sel:?} @ {scale}"
                );
            }
        }
    }

    #[test]
    fn has_every_tool_and_action_once() {
        let s = layout(Rect::new(600.0, 200.0, 800.0, 400.0), SCREEN, 1.0);
        assert_eq!(s.cells.len(), 19);
        assert_eq!(s.separators.len(), 3);
        let first = s.cells[0].1;
        assert_eq!(s.hit(first.center()), Some(StripItem::Tool(Tool::Select)));
        assert_eq!(s.hit(Point::new(first.x - 100.0, first.y)), None);
    }

    #[test]
    fn copy_text_sits_next_to_copy() {
        let s = layout(Rect::new(600.0, 200.0, 800.0, 400.0), SCREEN, 1.0);
        let items: Vec<StripItem> = s.cells.iter().map(|(item, _)| *item).collect();
        let at = items.iter().position(|i| *i == StripItem::Copy).unwrap();
        assert_eq!(items[at + 1], StripItem::CopyText);
        assert_eq!(StripItem::CopyText.name(), "Copy the text and close");
    }
}
