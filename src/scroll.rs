//! Smooth wheel scrolling.
//!
//! A mouse wheel turns in notches, and GTK moves a whole notch (some 70px)
//! in one frame, which reads as stutter. Here each notch glides over a few
//! frames instead, the way a browser scrolls. Touchpads already scroll
//! pixel by pixel and keep GTK's own handling.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};

/// How long one notch takes, in µs; another notch on the way extends it.
const GLIDE: f64 = 200_000.0;

/// Makes the mouse wheel glide `scroller` rather than jump it.
pub fn smooth_wheel(scroller: &gtk::ScrolledWindow) {
    let glide = Rc::new(RefCell::new(Glide::default()));
    let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    // ahead of the scrolled window's own handler, which would jump
    wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = scroller.downgrade();
    wheel.connect_scroll(move |wheel, _, dy| {
        let Some(scroller) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        // Shift and Ctrl change what the wheel does: GTK's business
        let held = gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::CONTROL_MASK;
        if wheel.unit() != gdk::ScrollUnit::Wheel
            || dy == 0.0
            || wheel.current_event_state().intersects(held)
        {
            // a touchpad taking over ends the glide where it is
            glide.borrow_mut().stop();
            return glib::Propagation::Proceed;
        }
        let adjustment = scroller.vadjustment();
        // GTK's own distance per notch: only the motion changes
        let step = adjustment.page_size().powf(2.0 / 3.0);
        let value = adjustment.value();
        let up = adjustment.lower() - value;
        let down = (adjustment.upper() - adjustment.page_size() - value).max(up);
        if glide.borrow_mut().push(dy * step, (up, down)) {
            let glide = glide.clone();
            scroller.add_tick_callback(move |scroller, clock| {
                let now = clock.frame_time();
                let frame = clock.refresh_info(now).0;
                let (delta, more) = glide.borrow_mut().step(now, frame);
                // by how much, not to where: GtkTextView moves the value
                // itself as it measures lines, and that must stay
                let adjustment = scroller.vadjustment();
                adjustment.set_value(adjustment.value() + delta);
                if more {
                    glib::ControlFlow::Continue
                } else {
                    glib::ControlFlow::Break
                }
            });
        }
        glib::Propagation::Stop
    });
    scroller.add_controller(wheel);
}

/// The motion itself, apart from GTK: an ease-out over `GLIDE`, restarted
/// from where it is whenever another notch comes in.
#[derive(Default)]
struct Glide {
    /// Pixels the current glide goes, and how many of them it went.
    distance: f64,
    covered: f64,
    /// Frame time it started at; `None` until its first frame.
    start: Option<i64>,
    /// Frame time of its latest frame.
    last: i64,
    ticking: bool,
}

impl Glide {
    /// Adds `delta` pixels to go. `room` is how far the view can move up
    /// (≤ 0) and down (≥ 0) from where it is. True when the glide starts
    /// now and needs frames.
    fn push(&mut self, delta: f64, room: (f64, f64)) -> bool {
        let mut left = self.distance - self.covered;
        // turning the wheel back stops at once rather than unwinding first
        if left * delta < 0.0 {
            left = 0.0;
        }
        self.distance = (left + delta).clamp(room.0, room.1);
        self.covered = 0.0;
        // a glide under way goes on from its latest frame: no frame stalls
        self.start = self.ticking.then_some(self.last);
        let starts = !self.ticking && self.distance != 0.0;
        self.ticking |= starts;
        starts
    }

    /// How far to move on the frame at `now`, `frame` µs after the one
    /// before, and whether more frames follow.
    fn step(&mut self, now: i64, frame: i64) -> (f64, bool) {
        // the first frame already moves: it is one frame into the glide
        let start = *self.start.get_or_insert(now - frame);
        self.last = now;
        let t = ((now - start) as f64 / GLIDE).clamp(0.0, 1.0);
        let covered = self.distance * (1.0 - (1.0 - t).powi(3));
        let delta = covered - self.covered;
        self.covered = covered;
        self.ticking = t < 1.0;
        (delta, self.ticking)
    }

    /// Ends the glide where it is.
    fn stop(&mut self) {
        self.distance = 0.0;
        self.covered = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: i64 = 16_667;
    const ROOM: (f64, f64) = (0.0, 10_000.0);

    /// Runs frames from `now` until the glide ends; the moves and the time.
    fn run(glide: &mut Glide, mut now: i64) -> (Vec<f64>, i64) {
        let mut moves = Vec::new();
        loop {
            let (delta, more) = glide.step(now, FRAME);
            moves.push(delta);
            if !more {
                return (moves, now);
            }
            now += FRAME;
        }
    }

    #[test]
    fn one_notch_glides_the_whole_way() {
        let mut glide = Glide::default();
        assert!(glide.push(70.0, ROOM), "a glide starts");
        let (moves, _) = run(&mut glide, 1_000_000);
        assert!(moves.len() >= 10, "spread over frames: {moves:?}");
        assert!(moves[0] > 0.0, "the first frame already moves");
        assert!(
            moves.windows(2).all(|m| m[1] <= m[0]),
            "eases out: {moves:?}"
        );
        assert!((moves.iter().sum::<f64>() - 70.0).abs() < 1e-9);
    }

    #[test]
    fn more_notches_add_up_without_a_stall() {
        let mut glide = Glide::default();
        glide.push(70.0, ROOM);
        let mut now = 1_000_000;
        let mut moved = 0.0;
        for _ in 0..3 {
            let (delta, more) = glide.step(now, FRAME);
            assert!(more && delta > 0.0);
            moved += delta;
            now += FRAME;
        }
        assert!(!glide.push(70.0, ROOM), "the running glide takes it");
        let (moves, _) = run(&mut glide, now);
        assert!(
            moves.iter().all(|m| *m > 0.0),
            "no frame stands still: {moves:?}"
        );
        moved += moves.iter().sum::<f64>();
        assert!((moved - 140.0).abs() < 1e-9, "moved {moved}");
    }

    #[test]
    fn turning_back_drops_the_rest() {
        let mut glide = Glide::default();
        glide.push(140.0, ROOM);
        let (first, _) = glide.step(1_000_000, FRAME);
        assert!(first > 0.0);
        glide.push(-70.0, (-10_000.0, 10_000.0));
        let (moves, _) = run(&mut glide, 1_000_000 + FRAME);
        assert!((moves.iter().sum::<f64>() + 70.0).abs() < 1e-9);
    }

    #[test]
    fn stops_at_the_ends() {
        let mut glide = Glide::default();
        assert!(
            !glide.push(70.0, (-500.0, 0.0)),
            "at the bottom: nothing to do"
        );
        assert!(glide.push(70.0, (-500.0, 30.0)));
        let (moves, _) = run(&mut glide, 1_000_000);
        assert!(
            (moves.iter().sum::<f64>() - 30.0).abs() < 1e-9,
            "only to the end"
        );
    }

    #[test]
    fn stopping_holds_still() {
        let mut glide = Glide::default();
        glide.push(70.0, ROOM);
        glide.step(1_000_000, FRAME);
        glide.stop();
        let (moves, _) = run(&mut glide, 1_000_000 + FRAME);
        assert!(moves.iter().all(|m| *m == 0.0), "{moves:?}");
    }
}
