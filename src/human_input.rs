//! Tether: human-like mouse and keyboard input for a remote session.
//!
//! Used by the Claude (MCP) control so the remote computer sees input the way a
//! person makes it: the pointer travels along a slightly curved path with a
//! minimum-jerk speed profile (fast in the middle, slow at both ends), a little
//! hand tremor, and on long moves a small overshoot that is then corrected;
//! clicks have a settle pause and a natural hold; wheel scrolling comes in
//! notches with irregular gaps; shortcuts press the modifiers first and release
//! them last. Text typing goes through `auto_type` (rhythm, pauses, typos).
use crate::{
    common::input::{
        MOUSE_BUTTON_LEFT, MOUSE_BUTTON_RIGHT, MOUSE_BUTTON_WHEEL, MOUSE_TYPE_DOWN,
        MOUSE_TYPE_MOVE, MOUSE_TYPE_UP, MOUSE_TYPE_WHEEL,
    },
    flutter::FlutterSession,
};
use hbb_common::rand::{rngs::StdRng, Rng, SeedableRng};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

#[derive(Debug)]
pub struct Interrupted;

/// Sleep in short slices so a stop request takes effect at once.
fn sleep(stop: &AtomicBool, seconds: f64) -> Result<(), Interrupted> {
    let end = std::time::Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err(Interrupted);
        }
        let now = std::time::Instant::now();
        if now >= end {
            return Ok(());
        }
        std::thread::sleep((end - now).min(Duration::from_millis(15)));
    }
}

fn gauss(rng: &mut StdRng, mu: f64, sigma: f64) -> f64 {
    // Box-Muller
    let u1: f64 = rng.gen_range(f64::EPSILON..1.0);
    let u2: f64 = rng.gen::<f64>();
    mu + sigma * (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

fn clamp_gauss(rng: &mut StdRng, mu: f64, sigma: f64, lo: f64, hi: f64) -> f64 {
    gauss(rng, mu, sigma).clamp(lo, hi)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "" | "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "middle" | "wheel" => Some(Self::Middle),
            _ => None,
        }
    }
    fn bits(self) -> i32 {
        match self {
            Self::Left => MOUSE_BUTTON_LEFT,
            Self::Right => MOUSE_BUTTON_RIGHT,
            Self::Middle => MOUSE_BUTTON_WHEEL,
        }
    }
}

// ---------------------------------------------------------------- pointer paths

/// One step of a movement: where to put the pointer, then how long to wait.
#[derive(Clone, Copy, Debug)]
pub struct PathStep {
    pub x: f64,
    pub y: f64,
    pub dt: f64,
}

fn bezier(p: [(f64, f64); 4], t: f64) -> (f64, f64) {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (
        a * p[0].0 + b * p[1].0 + c * p[2].0 + d * p[3].0,
        a * p[0].1 + b * p[1].1 + c * p[2].1 + d * p[3].1,
    )
}

/// Minimum-jerk progress: slow start, fast middle, slow end.
fn min_jerk(t: f64) -> f64 {
    t * t * t * (10.0 - 15.0 * t + 6.0 * t * t)
}

/// A single curved stroke from `from` to `to` taking about `duration` seconds.
fn stroke(
    rng: &mut StdRng,
    from: (f64, f64),
    to: (f64, f64),
    duration: f64,
    out: &mut Vec<PathStep>,
) {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let dist = (dx * dx + dy * dy).sqrt();
    if dist < 1.0 {
        out.push(PathStep { x: to.0, y: to.1, dt: 0.0 });
        return;
    }
    // Unit normal for the bow of the arc; both control points bend the same way
    // most of the time (a hand sweeps in an arc), sometimes an S.
    let (nx, ny) = (-dy / dist, dx / dist);
    let bow = gauss(rng, 0.0, dist * 0.10).clamp(-dist * 0.3, dist * 0.3);
    let bow2 = if rng.gen_bool(0.8) {
        bow * rng.gen_range(0.5..1.1)
    } else {
        -bow * rng.gen_range(0.2..0.6)
    };
    let a = rng.gen_range(0.2..0.4);
    let b = rng.gen_range(0.6..0.8);
    let p = [
        from,
        (from.0 + dx * a + nx * bow, from.1 + dy * a + ny * bow),
        (from.0 + dx * b + nx * bow2, from.1 + dy * b + ny * bow2),
        to,
    ];
    let mut elapsed = 0.0;
    let mut prev = from;
    while elapsed < duration {
        let dt = rng.gen_range(0.008..0.016);
        elapsed = (elapsed + dt).min(duration);
        let t = elapsed / duration;
        let (mut x, mut y) = bezier(p, min_jerk(t));
        // Tremor: strongest mid-move, nothing at the ends.
        let amp = 0.7 * (1.0 - (2.0 * t - 1.0).abs());
        x += gauss(rng, 0.0, amp);
        y += gauss(rng, 0.0, amp);
        if t >= 1.0 {
            (x, y) = to;
        }
        if (x - prev.0).abs() >= 0.5 || (y - prev.1).abs() >= 0.5 || t >= 1.0 {
            out.push(PathStep { x, y, dt });
            prev = (x, y);
        } else if let Some(last) = out.last_mut() {
            last.dt += dt;
        }
    }
}

/// Seconds a person takes to point at something `dist` pixels away (Fitts-like).
fn move_duration(rng: &mut StdRng, dist: f64) -> f64 {
    let base = 0.09 + 0.085 * (1.0 + dist / 10.0).log2();
    (base * clamp_gauss(rng, 1.0, 0.12, 0.75, 1.3)).clamp(0.07, 1.3)
}

/// The full human path from `from` to `to`, with an overshoot-and-correct on
/// long moves. The last step lands exactly on `to`.
pub fn human_path(rng: &mut StdRng, from: (f64, f64), to: (f64, f64), slow: f64) -> Vec<PathStep> {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let dist = (dx * dx + dy * dy).sqrt();
    let mut out = Vec::new();
    if dist < 1.0 {
        out.push(PathStep { x: to.0, y: to.1, dt: 0.0 });
        return out;
    }
    let duration = move_duration(rng, dist) * slow;
    if dist > 250.0 && rng.gen_bool(0.35) {
        let over = dist * rng.gen_range(0.025..0.06);
        let side = gauss(rng, 0.0, over * 0.4);
        let (ux, uy) = (dx / dist, dy / dist);
        let past = (
            to.0 + ux * over - uy * side,
            to.1 + uy * over + ux * side,
        );
        stroke(rng, from, past, duration, &mut out);
        // A beat to notice the overshoot, then a short correction.
        if let Some(last) = out.last_mut() {
            last.dt += rng.gen_range(0.04..0.11);
        }
        let back = rng.gen_range(0.10..0.20) * slow;
        stroke(rng, past, to, back, &mut out);
    } else {
        stroke(rng, from, to, duration, &mut out);
    }
    out
}

// ---------------------------------------------------------------- the driver

pub struct HumanInput<'a> {
    pub session: &'a FlutterSession,
    pub stop: &'a AtomicBool,
    rng: StdRng,
    /// Where we believe the remote pointer is (remote screen coordinates).
    pos: (f64, f64),
}

impl<'a> HumanInput<'a> {
    pub fn new(session: &'a FlutterSession, stop: &'a AtomicBool, pos: (f64, f64)) -> Self {
        Self {
            session,
            stop,
            rng: StdRng::from_entropy(),
            pos,
        }
    }

    pub fn position(&self) -> (f64, f64) {
        self.pos
    }

    fn mouse(&self, mask: i32, x: i32, y: i32) {
        self.session.send_mouse(mask, x, y, false, false, false, false);
    }

    pub fn move_to(&mut self, x: f64, y: f64) -> Result<(), Interrupted> {
        self.move_to_speed(x, y, 1.0)
    }

    fn move_to_speed(&mut self, x: f64, y: f64, slow: f64) -> Result<(), Interrupted> {
        let path = human_path(&mut self.rng, self.pos, (x, y), slow);
        let mut last = (i32::MIN, i32::MIN);
        for s in path {
            let p = (s.x.round() as i32, s.y.round() as i32);
            if p != last {
                self.mouse(MOUSE_TYPE_MOVE, p.0, p.1);
                last = p;
            }
            sleep(self.stop, s.dt)?;
        }
        self.pos = (x, y);
        Ok(())
    }

    fn press(&mut self, button: Button, down: bool) {
        let t = if down { MOUSE_TYPE_DOWN } else { MOUSE_TYPE_UP };
        let (x, y) = (self.pos.0.round() as i32, self.pos.1.round() as i32);
        self.mouse(button.bits() << 3 | t, x, y);
    }

    /// Move to (x, y) and click `count` times (1 = click, 2 = double click).
    pub fn click(&mut self, x: f64, y: f64, button: Button, count: u32) -> Result<(), Interrupted> {
        self.move_to(x, y)?;
        // Settle on the target before pressing.
        let settle = clamp_gauss(&mut self.rng, 0.12, 0.05, 0.05, 0.30);
        sleep(self.stop, settle)?;
        for i in 0..count.clamp(1, 3) {
            if i > 0 {
                let gap = clamp_gauss(&mut self.rng, 0.12, 0.03, 0.07, 0.20);
                sleep(self.stop, gap)?;
            }
            self.press(button, true);
            let hold = clamp_gauss(&mut self.rng, 0.085, 0.02, 0.045, 0.16);
            let r = sleep(self.stop, hold);
            self.press(button, false);
            r?;
        }
        Ok(())
    }

    pub fn drag(
        &mut self,
        from: (f64, f64),
        to: (f64, f64),
        button: Button,
    ) -> Result<(), Interrupted> {
        self.move_to(from.0, from.1)?;
        sleep(self.stop, clamp_gauss(&mut self.rng, 0.13, 0.04, 0.06, 0.25))?;
        self.press(button, true);
        let r = (|| {
            sleep(self.stop, clamp_gauss(&mut self.rng, 0.16, 0.05, 0.08, 0.30))?;
            // People drag a bit slower and more carefully than they point.
            self.move_to_speed(to.0, to.1, 1.35)?;
            sleep(self.stop, clamp_gauss(&mut self.rng, 0.12, 0.04, 0.06, 0.25))
        })();
        self.press(button, false);
        r
    }

    /// Wheel notches at (x, y). `dy` > 0 scrolls up, < 0 down; `dx` > 0 left, < 0 right
    /// (Tether's wheel convention).
    pub fn scroll(&mut self, x: f64, y: f64, dx: i32, dy: i32, notches: u32) -> Result<(), Interrupted> {
        self.move_to(x, y)?;
        sleep(self.stop, clamp_gauss(&mut self.rng, 0.10, 0.04, 0.04, 0.22))?;
        let mut burst: u32 = self.rng.gen_range(3..=6);
        for i in 0..notches.clamp(1, 50) {
            if i > 0 {
                let gap = if burst == 0 {
                    burst = self.rng.gen_range(3..=6);
                    self.rng.gen_range(0.15..0.35)
                } else {
                    clamp_gauss(&mut self.rng, 0.07, 0.025, 0.03, 0.15)
                };
                sleep(self.stop, gap)?;
            }
            self.mouse(MOUSE_TYPE_WHEEL, dx, dy);
            burst = burst.saturating_sub(1);
        }
        Ok(())
    }

    /// Press shortcuts like "ctrl+s", "alt+tab", "enter", or several separated
    /// by spaces ("ctrl+a delete"). Modifiers go down first and come up last.
    pub fn press_keys(&mut self, spec: &str) -> Result<(), String> {
        let combos = parse_keys(spec)?;
        for (i, combo) in combos.iter().enumerate() {
            if i > 0 {
                let gap = self.rng.gen_range(0.15..0.35);
                sleep(self.stop, gap).map_err(|_| "stopped".to_owned())?;
            }
            self.combo(combo).map_err(|_| "stopped".to_owned())?;
        }
        Ok(())
    }

    fn key(&self, name: &str, down: bool, m: &Mods) {
        self.session
            .input_key(name, down, false, m.alt, m.ctrl, m.shift, m.meta);
    }

    fn combo(&mut self, c: &Combo) -> Result<(), Interrupted> {
        let mut held = Mods::default();
        let mut pressed: Vec<&'static str> = Vec::new();
        let mut result = Ok(());
        for &(name, set) in &c.mods {
            set(&mut held);
            self.key(name, true, &held);
            pressed.push(name);
            if let Err(e) = sleep(self.stop, self.rng.gen_range(0.04..0.10)) {
                result = Err(e);
                break;
            }
        }
        if result.is_ok() {
            self.key(&c.key, true, &held);
            let hold = clamp_gauss(&mut self.rng, 0.09, 0.02, 0.05, 0.16);
            result = sleep(self.stop, hold);
            self.key(&c.key, false, &held);
        }
        // Always release what we pressed, in reverse order.
        for name in pressed.into_iter().rev() {
            let _ = sleep(self.stop, self.rng.gen_range(0.03..0.08));
            self.key(name, false, &held);
        }
        result
    }
}

#[derive(Default, Clone, Copy)]
struct Mods {
    alt: bool,
    ctrl: bool,
    shift: bool,
    meta: bool,
}

struct Combo {
    mods: Vec<(&'static str, fn(&mut Mods))>,
    key: String,
}

/// Parse "ctrl+shift+t enter" into combos. Key names: letters, digits and
/// punctuation as themselves; enter, tab, esc, backspace, delete, space, home,
/// end, pageup, pagedown, insert, up/down/left/right, f1-f12.
fn parse_keys(spec: &str) -> Result<Vec<Combo>, String> {
    let mut out = Vec::new();
    for part in spec.split_whitespace() {
        let tokens: Vec<&str> = if part == "+" {
            vec!["+"]
        } else {
            part.split('+').filter(|t| !t.is_empty()).collect()
        };
        if tokens.is_empty() {
            continue;
        }
        let mut combo = Combo { mods: vec![], key: String::new() };
        for (i, t) in tokens.iter().enumerate() {
            let lower = t.to_ascii_lowercase();
            let m: Option<(&'static str, fn(&mut Mods))> = match lower.as_str() {
                "ctrl" | "control" => Some(("VK_CONTROL", |m| m.ctrl = true)),
                "shift" => Some(("VK_SHIFT", |m| m.shift = true)),
                "alt" | "option" => Some(("VK_MENU", |m| m.alt = true)),
                "win" | "windows" | "cmd" | "command" | "meta" | "super" => {
                    Some(("Meta", |m| m.meta = true))
                }
                _ => None,
            };
            let last = i + 1 == tokens.len();
            match (m, last) {
                (Some(m), false) => combo.mods.push(m),
                (Some(m), true) => {
                    // A lone modifier ("win") is the key itself.
                    combo.key = m.0.to_owned();
                }
                (None, true) => combo.key = key_name(&lower, t)?,
                (None, false) => return Err(format!("'{t}' is not a modifier (ctrl, shift, alt, win)")),
            }
        }
        out.push(combo);
    }
    if out.is_empty() {
        return Err("no keys given".into());
    }
    Ok(out)
}

fn key_name(lower: &str, original: &str) -> Result<String, String> {
    let named = match lower {
        "enter" | "return" => "VK_RETURN",
        "tab" => "VK_TAB",
        "esc" | "escape" => "VK_ESCAPE",
        "backspace" | "back" => "VK_BACK",
        "delete" | "del" => "VK_DELETE",
        "space" => "VK_SPACE",
        "home" => "VK_HOME",
        "end" => "VK_END",
        "pageup" | "pgup" => "VK_PRIOR",
        "pagedown" | "pgdn" => "VK_NEXT",
        "insert" | "ins" => "VK_INSERT",
        "up" => "VK_UP",
        "down" => "VK_DOWN",
        "left" => "VK_LEFT",
        "right" => "VK_RIGHT",
        "capslock" => "VK_CAPITAL",
        "printscreen" | "prtsc" => "VK_SNAPSHOT",
        "menu" | "apps" => "Apps",
        _ => "",
    };
    if !named.is_empty() {
        return Ok(named.to_owned());
    }
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=12).contains(&n) {
            return Ok(format!("VK_F{n}"));
        }
    }
    let mut chars = original.chars();
    match (chars.next(), chars.next()) {
        // Single characters go as themselves (lower case for letters, so
        // "ctrl+S" doesn't turn into ctrl+shift+s).
        (Some(c), None) => Ok(c.to_ascii_lowercase().to_string()),
        _ => Err(format!("unknown key '{original}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_lands_on_target_and_takes_human_time() {
        let mut rng = StdRng::seed_from_u64(7);
        for &(to, min, max) in &[((800.0, 600.0), 0.25, 1.6), ((30.0, 10.0), 0.07, 0.6)] {
            let path = human_path(&mut rng, (0.0, 0.0), to, 1.0);
            let last = path.last().unwrap();
            assert_eq!((last.x, last.y), to);
            let total: f64 = path.iter().map(|s| s.dt).sum();
            assert!(total >= min && total <= max, "{to:?} took {total}");
            // No teleport: consecutive points stay close together.
            let mut prev = (0.0, 0.0);
            for s in &path {
                let jump = ((s.x - prev.0).powi(2) + (s.y - prev.1).powi(2)).sqrt();
                assert!(jump < 120.0, "jump of {jump}px");
                prev = (s.x, s.y);
            }
        }
    }

    #[test]
    fn path_is_curved_not_straight() {
        let mut rng = StdRng::seed_from_u64(3);
        let mut max_off: f64 = 0.0;
        for _ in 0..20 {
            for s in human_path(&mut rng, (0.0, 0.0), (1000.0, 0.0), 1.0) {
                max_off = max_off.max(s.y.abs());
            }
        }
        assert!(max_off > 5.0);
    }

    #[test]
    fn parses_shortcuts() {
        let c = parse_keys("ctrl+shift+t").unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].mods.len(), 2);
        assert_eq!(c[0].key, "t");
        let c = parse_keys("ctrl+a delete Enter F5 win").unwrap();
        let keys: Vec<_> = c.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["a", "VK_DELETE", "VK_RETURN", "VK_F5", "Meta"]);
        assert!(parse_keys("ctrl+").is_ok());
        assert!(parse_keys("foo+a").is_err());
        assert!(parse_keys("nosuchkey").is_err());
        assert_eq!(parse_keys("ctrl+S").unwrap()[0].key, "s");
    }
}
