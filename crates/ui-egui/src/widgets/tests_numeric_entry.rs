//! Real egui pointer and keyboard frames on the production slider readout.
use super::{SliderOut, slider, take_registry};
use crate::headless::HeadlessView;
use egui::{Event, Key, Modifiers, PointerButton, Rect, vec2};
use lcc_develop::{ControlSpec, controls};

struct Harness {
    view: HeadlessView,
    spec: ControlSpec,
    value: f64,
    enabled: bool,
    time: f64,
    rect: Rect,
    track: Rect,
    decoy: Option<f64>,
}
impl Harness {
    fn new() -> Self {
        let mut h = Self {
            view: HeadlessView::new(),
            spec: *controls::find("light.exposure").unwrap(),
            value: 0.71,
            enabled: true,
            time: 0.0,
            rect: Rect::NOTHING,
            track: Rect::NOTHING,
            decoy: None,
        };
        for _ in 0..3 {
            h.frame(vec![]);
        }
        h
    }
    fn frame(&mut self, events: Vec<Event>) -> SliderOut {
        self.time += 0.02;
        let mut out = SliderOut::default();
        self.view.run(HeadlessView::raw_input(vec2(330.0, 100.0), 1.0, self.time, events), |ui| {
            if let Some(value) = self.decoy {
                let other = slider(ui, controls::find("light.contrast").unwrap(), value, true, None);
                if let Some(value) = other.value {
                    self.decoy = Some(value);
                }
            }
            out = slider(ui, &self.spec, self.value, self.enabled, None);
        });
        let registry = take_registry(&self.view.ctx);
        let find = |name: String| registry.iter().find(|(id, _)| id == &name).map(|(_, r)| *r);
        self.rect = find(format!("sliderValue:{}", self.spec.id)).unwrap();
        self.track = find(format!("slider:{}", self.spec.id)).unwrap();
        if let Some(v) = out.value {
            self.value = v;
        }
        out
    }
    fn key(&mut self, key: Key) -> SliderOut {
        self.frame(vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }])
    }
}

#[test]
fn opening_release_honors_return_and_escape_immediately() {
    for key in [Key::Enter, Key::Escape] {
        let mut h = Harness::new();
        let pos = h.rect.center();
        h.frame(vec![Event::PointerMoved(pos)]);
        h.frame(vec![Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }]);
        let out = h.frame(vec![
            Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
            Event::Text("2".into()),
            Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE },
        ]);
        assert_eq!(out.value, (key == Key::Enter).then_some(2.0));
        assert!(!h.view.ctx.egui_wants_keyboard_input());
        assert!(h.key(Key::Enter).value.is_none(), "no retired draft can commit later");
    }
}

#[test]
fn dragging_away_from_readout_does_not_capture_keyboard() {
    // straight down: no sideways travel, so the press neither types nor scrubs
    let mut h = Harness::new();
    let inside = h.rect.center();
    let outside = egui::pos2(inside.x, 80.0);
    let button = |pos, pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    for events in [vec![Event::PointerMoved(inside)], vec![button(inside, true)], vec![Event::PointerMoved(outside)], vec![button(outside, false)]] {
        let out = h.frame(events);
        assert_eq!(out.value, None, "a cancelled readout gesture must not emit a value");
        assert_eq!(h.value, 0.71, "a cancelled readout gesture must preserve the stored value");
    }
    assert!(!h.view.ctx.egui_wants_keyboard_input(), "a cancelled readout gesture must not retain keyboard focus");
}

#[test]
fn typing_after_a_released_click_still_edits() {
    // Issue #1362: a normal click (press, a few frames held, release) must leave the field
    // editing, so the value can be typed without holding the button down.
    let mut h = Harness::new();
    let pos = h.rect.center();
    let button = |pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    h.frame(vec![Event::PointerMoved(pos)]);
    h.frame(vec![button(true)]);
    for _ in 0..4 {
        h.frame(vec![]);
    }
    assert_eq!(h.frame(vec![button(false)]).value, None);
    h.frame(vec![]);
    assert!(h.view.ctx.egui_wants_keyboard_input(), "the field keeps the keyboard after the click");
    h.frame(vec![Event::Text("2".into())]);
    assert_eq!(h.key(Key::Enter).value, Some(2.0));
}

#[test]
fn a_long_click_keeps_the_field_open() {
    // Issue #59: a press held past egui's click time (here a second and a half) used to close
    // the field on release; it stays open for typing.
    let mut h = Harness::new();
    let pos = h.rect.center();
    let button = |pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    h.frame(vec![Event::PointerMoved(pos)]);
    h.frame(vec![button(true)]);
    for _ in 0..75 {
        h.frame(vec![]);
    }
    assert_eq!(h.frame(vec![button(false)]).value, None);
    h.frame(vec![]);
    assert!(h.view.ctx.egui_wants_keyboard_input(), "the field keeps the keyboard after a long click");
    h.frame(vec![Event::Text("2".into())]);
    assert_eq!(h.key(Key::Enter).value, Some(2.0));
}

impl Harness {
    /// The track's width: a scrub moves the value as far as dragging the thumb would.
    fn track_width(&self) -> f32 {
        self.track.width()
    }
    /// Press on the value, move sideways by `dx` in `steps` frames (Shift held when `shift`) and release;
    /// the outputs of every frame.
    fn scrub(&mut self, dx: f32, steps: usize, shift: bool) -> Vec<SliderOut> {
        let modifiers = if shift { Modifiers::SHIFT } else { Modifiers::NONE };
        let start = self.rect.center();
        let button = |pos, pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers };
        let mut outs = vec![self.frame(vec![Event::ModifiersChanged(modifiers), Event::PointerMoved(start)]), self.frame(vec![button(start, true)])];
        for i in 1..=steps {
            let pos = start + vec2(dx * i as f32 / steps as f32, 0.0);
            outs.push(self.frame(vec![Event::PointerMoved(pos)]));
        }
        outs.push(self.frame(vec![button(start + vec2(dx, 0.0), false)]));
        outs.push(self.frame(vec![Event::ModifiersChanged(Modifiers::NONE)]));
        outs
    }
}

#[test]
fn dragging_the_value_sideways_scrubs_it_like_the_thumb() {
    let mut h = Harness::new();
    let width = f64::from(h.track_width());
    assert!(width > 100.0, "the slider has a track ({width})");
    // a fifth of the track: a fifth of the range (Exposure −5..+5 → +2 EV)
    let outs = h.scrub((width / 5.0) as f32, 10, false);
    assert!((h.value - 2.71).abs() < 0.015, "scrubbed to {}", h.value);
    assert_eq!(outs.iter().filter(|o| o.drag_started).count(), 1, "one interaction begins");
    assert_eq!(outs.iter().filter(|o| o.drag_stopped).count(), 1, "and ends, so the scrub is one undo step");
    assert!(outs.iter().filter_map(|o| o.value).count() > 1, "the value follows the pointer while it moves");
    assert!(!h.view.ctx.egui_wants_keyboard_input(), "a scrub opens no value field");
    assert!(h.key(Key::Enter).value.is_none(), "and leaves no draft to commit");
}

#[test]
fn scrubbing_back_and_with_shift() {
    let mut h = Harness::new();
    let width = h.track_width();
    // left lowers the value; on the control's steps and inside its range
    h.scrub(-width, 8, false);
    assert_eq!(h.value, -5.0, "clamped at the minimum");
    // Shift moves a tenth as far
    let mut h = Harness::new();
    h.scrub(width / 5.0, 10, true);
    assert!((h.value - 0.91).abs() < 0.015, "fine scrub to {}", h.value);
    assert!(((h.value / 0.01).round() * 0.01 - h.value).abs() < 1e-9, "on the 0.01 steps");
}

#[test]
fn a_press_that_barely_moves_still_types() {
    // a hand's jitter under the scrub distance is a click: the value can be typed
    let mut h = Harness::new();
    let pos = h.rect.center();
    let button = |pos, pressed| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    h.frame(vec![Event::PointerMoved(pos)]);
    h.frame(vec![button(pos, true)]);
    h.frame(vec![Event::PointerMoved(pos + vec2(1.5, 0.5))]);
    assert_eq!(h.frame(vec![button(pos + vec2(1.5, 0.5), false)]).value, None);
    h.frame(vec![]);
    assert!(h.view.ctx.egui_wants_keyboard_input(), "the field keeps the keyboard after the click");
    h.frame(vec![Event::Text("2".into())]);
    assert_eq!(h.key(Key::Enter).value, Some(2.0));
}

#[test]
fn a_disabled_value_does_not_scrub() {
    let mut h = Harness::new();
    h.enabled = false;
    let width = h.track_width();
    assert!(h.scrub(width / 5.0, 10, false).iter().all(|o| o.value.is_none() && !o.drag_started));
    assert_eq!(h.value, 0.71);
}

impl Harness {
    fn focus_value(&mut self) {
        let pos = self.rect.center();
        self.frame(vec![Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            self.frame(vec![Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }]);
        }
        self.frame(vec![]);
    }
}

#[test]
fn focused_number_arrows_apply_immediately_and_keep_focus() {
    let mut h = Harness::new();
    h.focus_value();
    let out = h.key(Key::ArrowUp);
    assert!((out.value.unwrap() - 0.81).abs() < 1e-9);
    assert!(h.view.painted_text().iter().any(|text| text.contains("0.81")), "the field shows the applied value");
    if let Some(path) = std::env::var_os("NUMERIC_ENTRY_SCREENSHOT") {
        let pixels = h.view.paint(&Default::default());
        let image = lcc_raster::Rgba8 { width: pixels.size[0], height: pixels.size[1], data: pixels.pixels.iter().map(|c| c.to_array()).collect() };
        let png = lcc_codecs::encode_png(&lcc_codecs::EncodeImage::rgba8(&image), &lcc_codecs::EncodeMeta::default()).unwrap();
        std::fs::write(path, png).unwrap();
    }
    assert!(out.drag_started && out.drag_stopped);
    assert!(h.view.ctx.egui_wants_keyboard_input());
    assert!((h.key(Key::ArrowDown).value.unwrap() - 0.71).abs() < 1e-9);
    assert!(h.key(Key::Enter).value.is_none(), "Return does not reapply a stale draft");
}

#[test]
fn focused_number_arrows_use_the_draft_and_clamp() {
    let mut h = Harness::new();
    h.focus_value();
    h.frame(vec![Event::Text("4.99".into())]);
    assert_eq!(h.key(Key::ArrowUp).value, Some(5.0));
    assert!(h.key(Key::ArrowUp).value.is_none());
    assert!((h.key(Key::ArrowDown).value.unwrap() - 4.9).abs() < 1e-9);
}

#[test]
fn exposure_shift_keeps_tenth_steps_on_repeat() {
    let mut h = Harness::new();
    h.focus_value();
    let out = h.frame(vec![
        Event::ModifiersChanged(Modifiers::SHIFT),
        Event::Key { key: Key::ArrowUp, physical_key: None, pressed: true, repeat: true, modifiers: Modifiers::SHIFT },
    ]);
    assert!((out.value.unwrap() - 0.81).abs() < 1e-9);
}

#[test]
fn exposure_ctrl_arrows_adjust_by_one_hundredth() {
    let mut h = Harness::new();
    h.focus_value();
    let modifiers = Modifiers { ctrl: true, command: true, ..Modifiers::NONE };
    for (key, expected) in [(Key::ArrowUp, 0.72), (Key::ArrowDown, 0.71)] {
        let out = h.frame(vec![Event::ModifiersChanged(modifiers), Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }]);
        assert!((out.value.unwrap() - expected).abs() < 1e-9);
        assert!(h.view.ctx.egui_wants_keyboard_input());
    }
}

#[test]
fn other_numeric_controls_keep_shift_steps() {
    let mut h = Harness::new();
    h.spec = *controls::find("light.contrast").unwrap();
    h.value = 10.0;
    h.frame(vec![]);
    h.focus_value();
    assert_eq!(h.key(Key::ArrowUp).value, Some(11.0));
    let modifiers = Modifiers::SHIFT;
    assert_eq!(
        h.frame(vec![
            Event::ModifiersChanged(modifiers),
            Event::Key { key: Key::ArrowDown, physical_key: None, pressed: true, repeat: false, modifiers }
        ])
        .value,
        Some(6.0)
    );
}

#[test]
fn disabled_numeric_field_ignores_arrows() {
    let mut h = Harness::new();
    h.focus_value();
    h.enabled = false;
    assert!(h.key(Key::ArrowUp).value.is_none());
    assert_eq!(h.value, 0.71);
}

#[test]
fn escape_discards_typing_after_an_applied_nudge() {
    let mut h = Harness::new();
    h.focus_value();
    h.key(Key::ArrowUp);
    let applied = h.value;
    h.frame(vec![Event::Text("3".into())]);
    assert!(h.key(Key::Escape).value.is_none());
    assert_eq!(h.value, applied);
    assert!(h.key(Key::Enter).value.is_none());
}

#[test]
fn focused_number_owns_arrows_while_another_slider_is_hovered() {
    let mut h = Harness::new();
    h.decoy = Some(10.0);
    h.frame(vec![]);
    h.focus_value();
    // Contrast is the preceding row; it must not steal the focused exposure field's keys.
    h.frame(vec![Event::PointerMoved(egui::pos2(h.track.center().x, 12.0))]);
    assert!((h.key(Key::ArrowUp).value.unwrap() - 0.81).abs() < 1e-9);
    assert_eq!(h.decoy, Some(10.0));
}
