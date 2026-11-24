//! Tether: "Auto type" for a remote session.
//!
//! Types a text into the remote machine one key at a time, like a person: variable timing,
//! thinking pauses, and occasional typos that are noticed and corrected with Backspace.
//! Port of the standalone "Auto Typing" app's engine (planner / humanizer / schedule).
//!
//! The keys travel as ordinary `KeyEvent` messages, so the remote side needs no change and the
//! local window does not need keyboard focus while typing.
//!
//! Windows peer: every character is a real key press. `KeyboardMode::Translate` with the
//! virtual-key code in the high word makes the controlled side inject the key's scan code
//! (see `translate_process_code` in `server/input_service.rs`); Shift is pressed as its own key.
//! The character -> key mapping uses this machine's keyboard layout. Characters with no plain
//! key (other scripts, emoji, AltGr characters) are sent as Unicode input instead.
//! Other peers: characters are sent as Unicode input, Enter / Backspace as control keys.
//!
//! Caps Lock: the events never carry the `CapsLock` lock mode, so the controlled side types
//! with Caps Lock off and restores it afterwards; the letter case comes from Shift alone.

use crate::{
    client::{Data, Interface},
    flutter::{sessions, FlutterHandler},
    ui_session_interface::Session,
};
use hbb_common::{
    log,
    message_proto::{ControlKey, KeyEvent, KeyboardMode, Message},
    rand::{rngs::StdRng, Rng, SeedableRng},
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};

use crate::flutter_ffi::SessionID;

lazy_static::lazy_static! {
    static ref JOBS: Mutex<HashMap<SessionID, Arc<Control>>> = Default::default();
}

const EVENT_NAME: &str = "auto_type";

// ---------------------------------------------------------------------------------------------
// Planner: the text as a list of actions (type / backspace / notice), including mistakes and
// their corrections. Pure logic, no timing.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Type,
    Backspace,
    /// The typist realises there is a mistake (a pause before the Backspaces).
    Notice,
}

#[derive(Clone, Copy, Debug)]
struct Action {
    kind: Kind,
    ch: char,
    /// Position in the source text this action relates to.
    index: usize,
    /// Counts towards progress (false for text that will be erased).
    advance: bool,
}

impl Action {
    fn typed(ch: char, index: usize, advance: bool) -> Self {
        Self {
            kind: Kind::Type,
            ch,
            index,
            advance,
        }
    }

    fn other(kind: Kind, index: usize) -> Self {
        Self {
            kind,
            ch: '\0',
            index,
            advance: false,
        }
    }
}

fn normalize(text: &str) -> Vec<char> {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .collect()
}

/// QWERTY rows with their horizontal offset, used to pick believable typos.
const ROWS: [(&str, f64); 4] = [
    ("1234567890", 0.0),
    ("qwertyuiop", 0.5),
    ("asdfghjkl", 0.75),
    ("zxcvbnm", 1.25),
];

fn key_position(ch: char) -> Option<(f64, f64)> {
    ROWS.iter().enumerate().find_map(|(row, (chars, offset))| {
        chars
            .chars()
            .position(|c| c == ch)
            .map(|i| (i as f64 + offset, row as f64))
    })
}

fn neighbors(ch: char) -> Vec<char> {
    let ch = ch.to_ascii_lowercase();
    let Some((x, y)) = key_position(ch) else {
        return Vec::new();
    };
    ROWS.iter()
        .flat_map(|(chars, _)| chars.chars())
        .filter(|&c| c != ch)
        .filter(|&c| match key_position(c) {
            Some((x2, y2)) => {
                let (dx, dy) = ((x - x2).abs(), (y - y2).abs());
                (dy == 0.0 && dx <= 1.01) || (dy == 1.0 && dx <= 1.0)
            }
            None => false,
        })
        .collect()
}

fn has_neighbor(ch: char) -> bool {
    !neighbors(ch).is_empty()
}

fn neighbor_of(ch: char, rng: &mut StdRng) -> char {
    let near = neighbors(ch);
    if near.is_empty() {
        return ch;
    }
    let pick = near[rng.gen_range(0..near.len())];
    if ch.is_uppercase() {
        pick.to_ascii_uppercase()
    } else {
        pick
    }
}

fn weighted<T: Copy>(rng: &mut StdRng, items: &[(T, f64)]) -> T {
    let total: f64 = items.iter().map(|(_, w)| w).sum();
    let mut r = rng.gen::<f64>() * total;
    for (item, w) in items {
        if r < *w {
            return *item;
        }
        r -= w;
    }
    items[items.len() - 1].0
}

#[derive(Clone, Copy, PartialEq)]
enum MistakeKind {
    Neighbor,
    Swap,
    Double,
    Skip,
}

const MISTAKE_KINDS: [(MistakeKind, f64); 4] = [
    (MistakeKind::Neighbor, 0.50),
    (MistakeKind::Swap, 0.20),
    (MistakeKind::Double, 0.15),
    (MistakeKind::Skip, 0.15),
];
/// How many correct characters get typed before the typist notices the mistake.
const LOOKAHEAD: [(usize, f64); 5] = [(0, 0.35), (1, 0.30), (2, 0.20), (3, 0.10), (4, 0.05)];

fn feasible(kind: MistakeKind, text: &[char], i: usize) -> bool {
    let ch = text[i];
    match kind {
        MistakeKind::Neighbor => has_neighbor(ch),
        MistakeKind::Swap => {
            i + 1 < text.len()
                && ch.is_alphabetic()
                && text[i + 1].is_alphabetic()
                && text[i + 1] != ch
        }
        MistakeKind::Double | MistakeKind::Skip => ch.is_alphanumeric(),
    }
}

struct Mistake {
    /// Characters typed instead of the intended ones, with the source index of each.
    wrong: Vec<(char, usize)>,
    /// First source index after the characters the mistake replaced.
    src_end: usize,
    /// Number of correct characters typed (from `src_end`) before the typist noticed.
    extra: usize,
}

fn make_mistake(text: &[char], i: usize, rng: &mut StdRng) -> Option<Mistake> {
    let mut kind = weighted(rng, &MISTAKE_KINDS);
    if !feasible(kind, text, i) {
        kind = MistakeKind::Neighbor;
        if !feasible(kind, text, i) {
            return None;
        }
    }
    let ch = text[i];
    let (wrong, src_end) = match kind {
        MistakeKind::Neighbor => (vec![(neighbor_of(ch, rng), i)], i + 1),
        MistakeKind::Swap => (vec![(text[i + 1], i), (ch, i + 1)], i + 2),
        MistakeKind::Double => (vec![(ch, i), (ch, i)], i + 1),
        MistakeKind::Skip => (vec![], i + 1),
    };
    let mut k = weighted(rng, &LOOKAHEAD);
    if kind == MistakeKind::Skip {
        // A skipped letter is only noticed once the next letters are typed.
        k = k.max(1);
    }
    let mut j = src_end;
    while j < text.len() && j - src_end < k && text[j] != '\n' {
        j += 1;
    }
    let extra = j - src_end;
    if kind == MistakeKind::Skip && extra == 0 {
        // Nothing to type after the skipped letter on this line: use a wrong-key typo instead.
        if !has_neighbor(ch) {
            return None;
        }
        return Some(Mistake {
            wrong: vec![(neighbor_of(ch, rng), i)],
            src_end: i + 1,
            extra: 0,
        });
    }
    Some(Mistake {
        wrong,
        src_end,
        extra,
    })
}

/// `error_rate`: chance per character of starting a mistake (0 disables mistakes).
fn plan(text: &[char], error_rate: f64, rng: &mut StdRng) -> Vec<Action> {
    let mut actions = Vec::with_capacity(text.len() * 11 / 10);
    let mut i = 0;
    while i < text.len() {
        if error_rate > 0.0 && rng.gen::<f64>() < error_rate {
            if let Some(m) = make_mistake(text, i, rng) {
                let end = m.src_end + m.extra;
                for &(c, idx) in &m.wrong {
                    actions.push(Action::typed(c, idx, false));
                }
                for j in m.src_end..end {
                    actions.push(Action::typed(text[j], j, false));
                }
                actions.push(Action::other(Kind::Notice, i));
                for _ in 0..(m.wrong.len() + m.extra) {
                    actions.push(Action::other(Kind::Backspace, i));
                }
                for j in i..end {
                    actions.push(Action::typed(text[j], j, true));
                }
                i = end;
                continue;
            }
        }
        actions.push(Action::typed(text[i], i, true));
        i += 1;
    }
    actions
}

// ---------------------------------------------------------------------------------------------
// Humanizer: how long a person waits between keys, holds a key, pauses to think.
// ---------------------------------------------------------------------------------------------

const FAST_DIGRAPHS: [&str; 26] = [
    "th", "he", "in", "er", "an", "re", "on", "at", "en", "nd", "ti", "es", "or", "te", "of",
    "ed", "is", "it", "al", "ar", "st", "to", "nt", "ng", "ou", "ha",
];
const SLOW_AFTER: &str = ",.;:!?)";

fn gauss(rng: &mut StdRng, mu: f64, sigma: f64) -> f64 {
    // Box-Muller
    let u1: f64 = rng.gen_range(f64::EPSILON..1.0);
    let u2: f64 = rng.gen();
    mu + sigma * (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

/// Log-normal factor with a mean of exactly 1.0.
fn lognormal(rng: &mut StdRng, sigma: f64) -> f64 {
    gauss(rng, -0.5 * sigma * sigma, sigma).exp()
}

struct Humanizer {
    wpm: f64,
    think_pauses: bool,
    /// Slowly drifting speed factor: typing comes in faster and slower stretches.
    rhythm: f64,
}

impl Humanizer {
    fn new(wpm: f64, think_pauses: bool) -> Self {
        Self {
            wpm: wpm.max(1.0),
            think_pauses,
            rhythm: 1.0,
        }
    }

    /// Mean seconds per character at the configured speed (5 characters = 1 word).
    fn base_interval(&self) -> f64 {
        60.0 / (self.wpm * 5.0)
    }

    /// Seconds from the previous key press to the press of `ch` (includes the hold time).
    fn interval(&mut self, rng: &mut StdRng, prev: Option<char>, ch: char) -> f64 {
        self.rhythm = (self.rhythm + gauss(rng, 0.0, 0.04) + (1.0 - self.rhythm) * 0.05)
            .clamp(0.78, 1.3);
        let mut t = self.base_interval() * lognormal(rng, 0.32) * self.rhythm;
        if let Some(prev) = prev {
            if prev == '\n' {
                t *= 2.2;
            } else if SLOW_AFTER.contains(prev) {
                t *= 1.7;
            } else if prev == ' ' {
                t *= 1.1;
            }
            let pair: String = [prev, ch].iter().flat_map(|c| c.to_lowercase()).collect();
            if FAST_DIGRAPHS.contains(&pair.as_str()) {
                t *= 0.8;
            }
            if prev == ch {
                t *= 0.9;
            }
        }
        if ch.is_uppercase() {
            t *= 1.25;
        } else if !ch.is_alphanumeric() && ch != ' ' && ch != '\n' {
            t *= 1.3;
        }
        t.max(0.02)
    }

    /// Extra pause, e.g. between words or sentences. 0 when none.
    fn think_pause(&self, rng: &mut StdRng, prev: Option<char>) -> f64 {
        if !self.think_pauses {
            return 0.0;
        }
        let (chance, lo, hi) = match prev {
            Some('\n') => (0.15, 0.4, 1.5),
            Some('.') | Some('!') | Some('?') => (0.08, 0.5, 1.8),
            Some(' ') => (0.012, 0.4, 1.6),
            _ => return 0.0,
        };
        if rng.gen::<f64>() < chance {
            rng.gen_range(lo..hi)
        } else {
            0.0
        }
    }

    /// How long a key stays pressed.
    fn hold(rng: &mut StdRng) -> f64 {
        gauss(rng, 0.075, 0.022).clamp(0.03, 0.16)
    }

    /// Time between making a mistake and realising it.
    fn notice_delay(rng: &mut StdRng) -> f64 {
        rng.gen_range(0.25..0.9)
    }

    fn backspace_interval(rng: &mut StdRng, first: bool) -> f64 {
        if first {
            rng.gen_range(0.10..0.30)
        } else {
            gauss(rng, 0.09, 0.025).max(0.04)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Schedule: timed steps, calibrated so the whole run averages the requested speed.
// Speed = (source characters / 5) per minute, including thinking pauses and corrections.
// ---------------------------------------------------------------------------------------------

/// How far the key intervals may be compressed before the pauses are squeezed instead.
const MIN_SCALE: f64 = 0.4;
const MAX_SCALE: f64 = 4.0;
/// A key is never held longer than this share of the interval.
const HOLD_FRACTION: f64 = 0.6;

#[derive(Clone, Copy, Debug)]
struct Step {
    action: Action,
    /// Gap before the key press (scalable).
    wait: f64,
    /// How long the key stays down.
    hold: f64,
    /// Pause that is part of the behaviour: thinking, noticing, backspace rhythm.
    fixed: f64,
}

fn raw_schedule(actions: &[Action], h: &mut Humanizer, rng: &mut StdRng) -> Vec<Step> {
    let mut steps = Vec::with_capacity(actions.len());
    let mut prev: Option<char> = None;
    let mut after_notice = false;
    let mut first = true;
    for &action in actions {
        let mut step = Step {
            action,
            wait: 0.0,
            hold: 0.0,
            fixed: 0.0,
        };
        match action.kind {
            Kind::Notice => {
                step.fixed = Humanizer::notice_delay(rng);
                steps.push(step);
                after_notice = true;
                continue;
            }
            Kind::Type => {
                if first {
                    step.hold = Humanizer::hold(rng);
                } else {
                    let interval = h.interval(rng, prev, action.ch);
                    step.hold = Humanizer::hold(rng).min(HOLD_FRACTION * interval);
                    step.wait = interval - step.hold;
                    step.fixed = h.think_pause(rng, prev);
                }
                prev = Some(action.ch);
            }
            Kind::Backspace => {
                step.hold = Humanizer::hold(rng);
                step.fixed = Humanizer::backspace_interval(rng, after_notice);
                prev = None;
            }
        }
        after_notice = false;
        first = false;
        steps.push(step);
    }
    steps
}

fn build_schedule(
    actions: &[Action],
    h: &mut Humanizer,
    rng: &mut StdRng,
    source_chars: usize,
) -> Vec<Step> {
    let mut steps = raw_schedule(actions, h, rng);
    let target = source_chars as f64 * h.base_interval();
    // Typing steps scale as a unit (interval = wait + hold); everything else is "fixed".
    let scalable: f64 = steps
        .iter()
        .filter(|s| s.action.kind == Kind::Type)
        .map(|s| s.wait + s.hold)
        .sum();
    let fixed: f64 = steps.iter().map(|s| s.fixed).sum::<f64>()
        + steps
            .iter()
            .filter(|s| s.action.kind != Kind::Type)
            .map(|s| s.hold)
            .sum::<f64>();
    if scalable <= 0.0 {
        return steps;
    }
    let mut k = (target - fixed) / scalable;
    let mut fixed_scale = 1.0;
    if k < MIN_SCALE {
        // Very high speed: the key intervals alone cannot absorb the pauses, so shrink those too.
        k = MIN_SCALE;
        if fixed > 0.0 {
            fixed_scale = ((target - k * scalable) / fixed).max(0.0);
        }
    }
    let k = k.min(MAX_SCALE);
    for s in steps.iter_mut() {
        if s.action.kind == Kind::Type {
            let interval = k * (s.wait + s.hold);
            s.hold = (s.hold * k.min(1.0)).min(HOLD_FRACTION * interval);
            s.wait = (interval - s.hold).max(0.0);
        } else {
            s.hold *= fixed_scale;
        }
        s.fixed *= fixed_scale;
    }
    steps
}

// ---------------------------------------------------------------------------------------------
// Keys: turn actions into KeyEvent messages for the remote session.
// ---------------------------------------------------------------------------------------------

const VK_BACK: u32 = 0x08;
const VK_TAB: u32 = 0x09;
const VK_RETURN: u32 = 0x0D;
const VK_LSHIFT: u32 = 0xA0;

/// The key that produces `ch` on this machine's keyboard layout: (virtual key, needs Shift).
/// `None` when there is no plain key for it (the character is then sent as Unicode).
#[cfg(windows)]
fn resolve_vk(ch: char) -> Option<(u32, bool)> {
    use winapi::um::winuser::{GetKeyboardLayout, VkKeyScanExW};
    match ch {
        '\n' => return Some((VK_RETURN, false)),
        '\t' => return Some((VK_TAB, false)),
        _ => {}
    }
    let code = u32::from(ch);
    if code > 0xFFFF || ch.is_control() {
        return None;
    }
    let result = unsafe { VkKeyScanExW(code as u16, GetKeyboardLayout(0)) };
    if result == -1 {
        return None;
    }
    let vk = (result as u16 & 0xFF) as u32;
    let state = (result as u16 >> 8) & 0xFF;
    // Only plain and Shift keys: Ctrl / Alt (AltGr) / Kana states go as Unicode.
    if vk == 0xFF || state & !1 != 0 {
        return None;
    }
    Some((vk, state & 1 != 0))
}

#[cfg(not(windows))]
fn resolve_vk(_ch: char) -> Option<(u32, bool)> {
    None
}

struct Keys {
    session: Weak<Session<FlutterHandler>>,
    /// The controlled machine runs Windows: send real key presses.
    windows_peer: bool,
    /// Virtual keys currently held down on the remote side.
    down: Vec<u32>,
}

impl Keys {
    fn send(&self, evt: KeyEvent) {
        if let Some(session) = self.session.upgrade() {
            let mut msg = Message::new();
            msg.set_key_event(evt);
            session.send(Data::Message(msg));
        }
    }

    fn vk(&mut self, vk: u32, down: bool) {
        let mut evt = KeyEvent::new();
        evt.down = down;
        evt.set_chr(vk << 16);
        evt.mode = KeyboardMode::Translate.into();
        self.send(evt);
        if down {
            self.down.push(vk);
        } else {
            self.down.retain(|&k| k != vk);
        }
    }

    fn unicode(&self, ch: char) {
        let mut evt = KeyEvent::new();
        evt.set_seq(ch.to_string());
        evt.mode = KeyboardMode::Translate.into();
        self.send(evt);
    }

    fn control_key(&self, key: ControlKey) {
        let mut evt = KeyEvent::new();
        evt.press = true;
        evt.set_control_key(key);
        evt.mode = KeyboardMode::Legacy.into();
        self.send(evt);
    }

    /// Press the key(s) for `ch`, keep them down for `hold` seconds, release.
    fn type_char(&mut self, ch: char, hold: f64, ctl: &Control) -> Result<(), Stopped> {
        if self.windows_peer {
            if let Some((vk, shift)) = resolve_vk(ch) {
                if shift {
                    self.vk(VK_LSHIFT, true);
                }
                self.vk(vk, true);
                let r = ctl.sleep(hold);
                self.vk(vk, false);
                if shift {
                    self.vk(VK_LSHIFT, false);
                }
                return r;
            }
        } else if ch == '\n' {
            self.control_key(ControlKey::Return);
            return ctl.sleep(hold);
        }
        self.unicode(ch);
        ctl.sleep(hold)
    }

    fn backspace(&mut self, hold: f64, ctl: &Control) -> Result<(), Stopped> {
        if !self.windows_peer {
            self.control_key(ControlKey::Backspace);
            return ctl.sleep(hold);
        }
        self.vk(VK_BACK, true);
        let r = ctl.sleep(hold);
        self.vk(VK_BACK, false);
        r
    }

    fn release_all(&mut self) {
        for vk in std::mem::take(&mut self.down).into_iter().rev() {
            self.vk(vk, false);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Job: one typing run per session, on its own thread.
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Stopped;

#[derive(Default)]
struct Control {
    stop: AtomicBool,
    paused: AtomicBool,
    /// Extra stop switch owned by the caller (the Claude control's Stop).
    external: Option<Arc<AtomicBool>>,
}

impl Control {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
            || self
                .external
                .as_ref()
                .is_some_and(|e| e.load(Ordering::SeqCst))
    }

    /// Sleep in short slices so Stop reacts immediately.
    fn sleep(&self, seconds: f64) -> Result<(), Stopped> {
        let end = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
        loop {
            if self.stopped() {
                return Err(Stopped);
            }
            let now = Instant::now();
            if now >= end {
                return Ok(());
            }
            std::thread::sleep((end - now).min(Duration::from_millis(20)));
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub wpm: f64,
    /// Chance per character of making a mistake, 0..=1.
    pub error_rate: f64,
    /// Seconds of countdown before the first key.
    pub start_delay: u32,
    pub think_pauses: bool,
}

struct Job {
    session_id: SessionID,
    session: Weak<Session<FlutterHandler>>,
    ctl: Arc<Control>,
    keys: Keys,
    total: usize,
    /// Source characters typed so far.
    done: usize,
    /// No status events to the session window (typing for the Claude control).
    quiet: bool,
}

impl Job {
    fn emit(&self, state: &str, extra: &str) {
        if self.quiet {
            return;
        }
        if let Some(session) = self.session.upgrade() {
            let done = self.done.to_string();
            let total = self.total.to_string();
            session.ui_handler.push_event(
                EVENT_NAME,
                &[
                    ("state", state),
                    ("done", done.as_str()),
                    ("total", total.as_str()),
                    ("extra", extra),
                ],
                &[],
            );
        }
    }

    fn connected(&self) -> bool {
        match self.session.upgrade() {
            Some(session) => session.sender.read().unwrap().is_some(),
            None => false,
        }
    }

    /// Honour stop and pause requests; blocks while paused (all keys released).
    /// Returns the seconds spent paused.
    fn checkpoint(&mut self) -> Result<f64, Stopped> {
        if self.ctl.stopped() || self.session.upgrade().is_none() {
            return Err(Stopped);
        }
        if !self.connected() && self.quiet {
            // Nobody watches a quiet run's pause state: give up instead.
            return Err(Stopped);
        }
        if !self.connected() {
            self.ctl.paused.store(true, Ordering::SeqCst);
            self.keys.release_all();
            self.emit("paused", "Connection lost");
        }
        if !self.ctl.paused.load(Ordering::SeqCst) {
            return Ok(0.0);
        }
        let began = Instant::now();
        self.keys.release_all();
        while self.ctl.paused.load(Ordering::SeqCst) {
            if self.ctl.stopped() || self.session.upgrade().is_none() {
                return Err(Stopped);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.emit("running", "");
        Ok(began.elapsed().as_secs_f64())
    }

    fn run(&mut self, text: Vec<char>, settings: Settings) {
        let result = self.run_inner(&text, settings);
        self.keys.release_all();
        match result {
            Ok(()) => {
                self.done = self.total;
                self.emit("done", "");
            }
            Err(Stopped) => self.emit("stopped", ""),
        }
        let mut jobs = JOBS.lock().unwrap();
        if jobs
            .get(&self.session_id)
            .is_some_and(|c| Arc::ptr_eq(c, &self.ctl))
        {
            jobs.remove(&self.session_id);
        }
    }

    fn run_inner(&mut self, text: &[char], settings: Settings) -> Result<(), Stopped> {
        let mut rng = StdRng::from_entropy();
        let actions = plan(text, settings.error_rate, &mut rng);
        let mut humanizer = Humanizer::new(settings.wpm, settings.think_pauses);
        let steps = build_schedule(&actions, &mut humanizer, &mut rng, text.len());

        for remaining in (1..=settings.start_delay).rev() {
            self.emit("countdown", &remaining.to_string());
            self.ctl.sleep(1.0)?;
            self.checkpoint()?;
        }
        self.emit("running", "");

        let ctl = self.ctl.clone();
        let started = Instant::now();
        let mut paused_total = 0.0;
        // Where the schedule says we should be by now.
        let mut planned = 0.0;
        for step in steps {
            let action = step.action;
            if action.kind == Kind::Notice {
                ctl.sleep(step.fixed)?;
                planned += step.fixed;
                continue;
            }
            // Speed control: sending and sleeping take a little longer than planned. Whenever we
            // are behind schedule, shorten the next gap so the run still averages the set speed.
            let mut wait = step.wait;
            let behind = started.elapsed().as_secs_f64() - paused_total - planned;
            if behind > 0.0 && wait > 0.0 {
                wait -= behind.min(wait * 0.9);
            }
            planned += step.wait + step.hold + step.fixed;

            ctl.sleep(wait + step.fixed)?;
            paused_total += self.checkpoint()?;
            match action.kind {
                Kind::Type => self.keys.type_char(action.ch, step.hold, &ctl)?,
                Kind::Backspace => self.keys.backspace(step.hold, &ctl)?,
                Kind::Notice => {}
            }
            if action.kind == Kind::Type && action.advance && action.index + 1 > self.done {
                self.done = action.index + 1;
                self.emit("running", "");
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// API used by flutter_ffi.rs
// ---------------------------------------------------------------------------------------------

pub fn start(session_id: SessionID, text: &str, settings: Settings) {
    let Some(session) = sessions::get_session_by_session_id(&session_id) else {
        return;
    };
    let text = normalize(text);
    let push_error = |msg: &str| {
        session.ui_handler.push_event(
            EVENT_NAME,
            &[("state", "error"), ("done", "0"), ("total", "0"), ("extra", msg)],
            &[],
        );
    };
    if text.is_empty() {
        push_error("There is no text to type");
        return;
    }
    if !*session.server_keyboard_enabled.read().unwrap() {
        push_error("Keyboard input is not allowed by the remote side");
        return;
    }
    let ctl = Arc::new(Control::default());
    {
        let mut jobs = JOBS.lock().unwrap();
        if jobs.contains_key(&session_id) {
            push_error("Auto type is already running");
            return;
        }
        jobs.insert(session_id, ctl.clone());
    }
    let windows_peer = session.peer_platform() == "Windows";
    let weak = Arc::downgrade(&session);
    let mut job = Job {
        session_id,
        session: weak.clone(),
        ctl,
        keys: Keys {
            session: weak,
            windows_peer,
            down: Vec::new(),
        },
        total: text.len(),
        done: 0,
        quiet: false,
    };
    let settings = Settings {
        wpm: settings.wpm.clamp(5.0, 300.0),
        error_rate: settings.error_rate.clamp(0.0, 0.3),
        start_delay: settings.start_delay.min(30),
        think_pauses: settings.think_pauses,
    };
    let spawned = std::thread::Builder::new()
        .name("auto-type".to_owned())
        .spawn(move || job.run(text, settings));
    if let Err(e) = spawned {
        log::error!("auto type: failed to start the typing thread: {}", e);
        JOBS.lock().unwrap().remove(&session_id);
        push_error("Failed to start");
    }
}

/// Type `text` on the calling thread, without status events (used by the
/// Claude control). Returns when done; `stop` aborts it.
pub fn type_blocking(
    session_id: SessionID,
    text: &str,
    settings: Settings,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let Some(session) = sessions::get_session_by_session_id(&session_id) else {
        return Err("the session is closed".into());
    };
    let text = normalize(text);
    if text.is_empty() {
        return Ok(());
    }
    if !*session.server_keyboard_enabled.read().unwrap() {
        return Err("keyboard input is not allowed by the remote side".into());
    }
    let ctl = Arc::new(Control {
        external: Some(stop),
        ..Default::default()
    });
    {
        let mut jobs = JOBS.lock().unwrap();
        if jobs.contains_key(&session_id) {
            return Err("Auto type is already running in this session".into());
        }
        jobs.insert(session_id, ctl.clone());
    }
    let windows_peer = session.peer_platform() == "Windows";
    let weak = Arc::downgrade(&session);
    drop(session);
    let mut job = Job {
        session_id,
        session: weak.clone(),
        ctl: ctl.clone(),
        keys: Keys {
            session: weak,
            windows_peer,
            down: Vec::new(),
        },
        total: text.len(),
        done: 0,
        quiet: true,
    };
    let settings = Settings {
        wpm: settings.wpm.clamp(5.0, 300.0),
        error_rate: settings.error_rate.clamp(0.0, 0.3),
        start_delay: 0,
        think_pauses: settings.think_pauses,
    };
    let result = job.run_inner(&text, settings);
    job.keys.release_all();
    let mut jobs = JOBS.lock().unwrap();
    if jobs.get(&session_id).is_some_and(|c| Arc::ptr_eq(c, &ctl)) {
        jobs.remove(&session_id);
    }
    result.map_err(|_| "stopped".into())
}

/// `action`: "pause", "resume" or "stop".
pub fn control(session_id: SessionID, action: &str) {
    let Some(ctl) = JOBS.lock().unwrap().get(&session_id).cloned() else {
        return;
    };
    match action {
        "pause" => ctl.paused.store(true, Ordering::SeqCst),
        "resume" => ctl.paused.store(false, Ordering::SeqCst),
        "stop" => ctl.stop.store(true, Ordering::SeqCst),
        _ => log::warn!("auto type: unknown action {}", action),
    }
    if action == "pause" {
        if let Some(session) = sessions::get_session_by_session_id(&session_id) {
            session.ui_handler.push_event(
                EVENT_NAME,
                &[("state", "paused"), ("extra", "")],
                &[],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(actions: &[Action]) -> String {
        let mut buf = Vec::new();
        for a in actions {
            match a.kind {
                Kind::Type => buf.push(a.ch),
                Kind::Backspace => {
                    buf.pop();
                }
                Kind::Notice => {}
            }
        }
        buf.into_iter().collect()
    }

    const SAMPLE: &str = "The quick brown fox jumps over the lazy dog.\nfn main() { println!(\"hi\"); }\n";

    #[test]
    fn plan_without_mistakes_types_the_text() {
        let text = normalize(SAMPLE);
        let mut rng = StdRng::seed_from_u64(1);
        let actions = plan(&text, 0.0, &mut rng);
        assert_eq!(actions.len(), text.len());
        assert_eq!(apply(&actions), SAMPLE);
    }

    #[test]
    fn corrected_mistakes_end_in_the_original_text() {
        let text = normalize(SAMPLE);
        for seed in 0..200 {
            let mut rng = StdRng::seed_from_u64(seed);
            let actions = plan(&text, 0.2, &mut rng);
            assert_eq!(apply(&actions), SAMPLE, "seed {}", seed);
        }
    }

    #[test]
    fn mistakes_happen_at_a_high_rate() {
        let text = normalize(SAMPLE);
        let mut rng = StdRng::seed_from_u64(7);
        let actions = plan(&text, 0.2, &mut rng);
        assert!(actions.iter().any(|a| a.kind == Kind::Backspace));
    }

    #[test]
    fn crlf_is_normalized() {
        assert_eq!(normalize("a\r\nb\rc"), vec!['a', '\n', 'b', '\n', 'c']);
    }

    #[test]
    fn neighbors_are_adjacent_keys() {
        let n = neighbors('g');
        for c in ['f', 'h', 't', 'y', 'v', 'b'] {
            assert!(n.contains(&c), "{} missing from {:?}", c, n);
        }
        assert!(!n.contains(&'a'));
        assert!(neighbors('#').is_empty());
        let mut rng = StdRng::seed_from_u64(3);
        assert!(neighbor_of('G', &mut rng).is_uppercase());
    }

    #[test]
    fn schedule_matches_the_requested_speed() {
        let text = normalize(&SAMPLE.repeat(5));
        for wpm in [20.0, 60.0, 120.0] {
            let mut rng = StdRng::seed_from_u64(42);
            let actions = plan(&text, 0.05, &mut rng);
            let mut h = Humanizer::new(wpm, true);
            let steps = build_schedule(&actions, &mut h, &mut rng, text.len());
            let total: f64 = steps.iter().map(|s| s.wait + s.hold + s.fixed).sum();
            let target = text.len() as f64 * 60.0 / (wpm * 5.0);
            assert!(
                (total - target).abs() / target < 0.02,
                "wpm {}: {} vs {}",
                wpm,
                total,
                target
            );
            assert!(steps.iter().all(|s| s.wait >= 0.0 && s.hold >= 0.0 && s.fixed >= 0.0));
        }
    }
}
