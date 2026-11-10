// Grid layout (right edge, top to bottom, then columns to the left, like macOS;
// or mirrored from the left edge when the user chooses that)
// and the expand/collapse animation.

use crate::model::{Kind, Model};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

#[derive(Clone, Debug, PartialEq)]
pub enum EntryRef {
    Stack(Kind),
    File(usize),
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub key: String,
    pub r: EntryRef,
}

pub fn stack_key(k: Kind) -> String {
    format!("S:{:?}", k)
}
pub fn file_key(path: &str) -> String {
    format!("F:{}", path)
}

/// Builds the visible cell sequence. A kind with one file is shown as that file.
pub fn build_entries(m: &Model, expanded: &HashSet<Kind>) -> Vec<Entry> {
    let mut out = Vec::new();
    for (k, idx) in &m.groups {
        if idx.len() == 1 {
            out.push(Entry { key: file_key(&m.files[idx[0]].path), r: EntryRef::File(idx[0]) });
            continue;
        }
        out.push(Entry { key: stack_key(*k), r: EntryRef::Stack(*k) });
        if expanded.contains(k) {
            for &i in idx {
                out.push(Entry { key: file_key(&m.files[i].path), r: EntryRef::File(i) });
            }
        }
    }
    for &i in &m.loose_tail {
        out.push(Entry { key: file_key(&m.files[i].path), r: EntryRef::File(i) });
    }
    out
}

/// Which stack each stacked file belongs to (file key -> stack key).
pub fn stack_membership(m: &Model) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (k, idx) in &m.groups {
        if idx.len() > 1 {
            for &i in idx {
                map.insert(file_key(&m.files[i].path), stack_key(*k));
            }
        }
    }
    map
}

#[derive(Clone, Copy, Debug)]
pub struct Geom {
    pub w: f32,
    pub h: f32,
    pub s: f32,
    pub cell_w: f32,
    pub cell_h: f32,
    pub icon: f32,
    pub margin_x: f32,
    pub margin_y: f32,
    pub rows: usize,
    /// Sideways slide in pixels, used when there are more columns than fit the screen.
    pub scroll: f32,
    /// Columns start at the left screen edge instead of the right one.
    pub left: bool,
}

impl Geom {
    pub fn new(w: f32, h: f32, scale: f32) -> Geom {
        Geom::with_options(w, h, scale, 1.0, false)
    }
    /// `size` is the user's icon size factor (1.0 = medium), `left` the screen edge.
    pub fn with_options(w: f32, h: f32, scale: f32, size: f32, left: bool) -> Geom {
        let size = if size.is_finite() { size.clamp(0.5, 2.0) } else { 1.0 };
        let s = scale.max(1.0) * size;
        let cell_w = 92.0 * s;
        let cell_h = 100.0 * s;
        let margin_y = 10.0 * s;
        let rows = (((h - 2.0 * margin_y) / cell_h).floor() as usize).max(1);
        Geom { w, h, s, cell_w, cell_h, icon: 56.0 * s, margin_x: 14.0 * s, margin_y, rows, scroll: 0.0, left }
    }
    pub fn cell_pos(&self, i: usize) -> (f32, f32) {
        let col = i / self.rows;
        let row = i % self.rows;
        let x = if self.left {
            self.margin_x + col as f32 * self.cell_w - self.scroll
        } else {
            self.w - self.margin_x - (col as f32 + 1.0) * self.cell_w + self.scroll
        };
        (x, self.margin_y + row as f32 * self.cell_h)
    }
    /// How far the columns can slide to bring the ones past the far screen edge into view
    /// (0 when everything fits).
    pub fn max_scroll(&self, n: usize) -> f32 {
        let cols = n.div_ceil(self.rows) as f32;
        (2.0 * self.margin_x + cols * self.cell_w - self.w).max(0.0)
    }
}

/// Mouse wheel over a closed stack: which file to show next.
/// One wheel notch (120 units) moves one file; small touchpad deltas add up in `acc`
/// until they make a notch. Wheel down goes to the next (older) file, wheel up back.
/// `cur` = None means the stack still shows its cover, which is file 0.
/// Stops at the first and last file. Returns None when there is nothing to change.
pub fn scrub_step(cur: Option<usize>, len: usize, acc: &mut i32, delta: i32) -> Option<usize> {
    const NOTCH: i32 = 120;
    if len == 0 {
        *acc = 0;
        return None;
    }
    *acc = acc.saturating_add(delta);
    let steps = *acc / NOTCH;
    if steps == 0 {
        return None;
    }
    *acc -= steps * NOTCH;
    let at = cur.unwrap_or(0).min(len - 1) as i64;
    Some((at - steps as i64).clamp(0, len as i64 - 1) as usize)
}

#[derive(Clone, Copy, Debug)]
pub struct Vis {
    pub x: f32,
    pub y: f32,
    pub a: f32,
    pub sc: f32,
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

pub struct Ghost {
    pub entry: Entry,
}

pub struct Animator {
    pub cur: HashMap<String, Vis>,
    from: HashMap<String, Vis>,
    to: HashMap<String, Vis>,
    pub ghosts: Vec<Ghost>,
    start: Instant,
    pub active: bool,
}

const DURATION: f32 = 0.24;

impl Animator {
    pub fn new() -> Self {
        Animator {
            cur: HashMap::new(),
            from: HashMap::new(),
            to: HashMap::new(),
            ghosts: Vec::new(),
            start: Instant::now(),
            active: false,
        }
    }

    /// Sets new targets. Files appearing out of a stack start at the stack;
    /// files folding back into a stack fly into it and fade.
    pub fn retarget(
        &mut self,
        g: &Geom,
        old: &[Entry],
        new: &[Entry],
        membership: &HashMap<String, String>,
        animate: bool,
    ) {
        let mut to = HashMap::new();
        for (i, e) in new.iter().enumerate() {
            let (x, y) = g.cell_pos(i);
            to.insert(e.key.clone(), Vis { x, y, a: 1.0, sc: 1.0 });
        }
        if !animate {
            self.cur = to.clone();
            self.to = to;
            self.ghosts.clear();
            self.active = false;
            return;
        }
        let mut from = HashMap::new();
        for e in new {
            let t = to[&e.key];
            let f = if let Some(c) = self.cur.get(&e.key) {
                *c
            } else if let Some(sk) = membership.get(&e.key) {
                let origin = self.cur.get(sk).or(to.get(sk)).copied().unwrap_or(t);
                Vis { x: origin.x, y: origin.y, a: 0.0, sc: 0.5 }
            } else {
                Vis { x: t.x, y: t.y, a: 0.0, sc: 0.8 }
            };
            from.insert(e.key.clone(), f);
        }
        self.ghosts.clear();
        let new_keys: HashSet<&String> = new.iter().map(|e| &e.key).collect();
        for e in old {
            if new_keys.contains(&e.key) {
                continue;
            }
            if let (Some(sk), Some(c)) = (membership.get(&e.key), self.cur.get(&e.key)) {
                if let Some(target) = to.get(sk) {
                    from.insert(e.key.clone(), *c);
                    to.insert(e.key.clone(), Vis { x: target.x, y: target.y, a: 0.0, sc: 0.5 });
                    self.ghosts.push(Ghost { entry: e.clone() });
                }
            }
        }
        self.from = from;
        self.to = to;
        self.cur = self.from.clone();
        self.start = Instant::now();
        self.active = true;
    }

    /// Jumps straight to the final state (used when drawing is suspended mid-animation).
    pub fn finish(&mut self) {
        for gh in self.ghosts.drain(..) {
            self.to.remove(&gh.entry.key);
        }
        self.cur = self.to.clone();
        self.active = false;
    }

    /// Advances the animation. Returns true while still running.
    pub fn tick(&mut self) -> bool {
        if !self.active {
            return false;
        }
        let t = (self.start.elapsed().as_secs_f32() / DURATION).min(1.0);
        let e = 1.0 - (1.0 - t).powi(3);
        for (k, to) in &self.to {
            let f = self.from.get(k).copied().unwrap_or(*to);
            self.cur.insert(
                k.clone(),
                Vis { x: lerp(f.x, to.x, e), y: lerp(f.y, to.y, e), a: lerp(f.a, to.a, e), sc: lerp(f.sc, to.sc, e) },
            );
        }
        if t >= 1.0 {
            for gh in self.ghosts.drain(..) {
                self.cur.remove(&gh.entry.key);
                self.to.remove(&gh.entry.key);
            }
            self.active = false;
        }
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FileItem, KIND_ORDER};

    // Small deterministic pseudo-random generator, so the tests need no extra crates.
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % n
        }
    }

    fn file(path: String, is_dir: bool, kind: Kind) -> FileItem {
        FileItem { name: path.clone(), path, mtime: 0, is_dir, is_system: false, kind }
    }

    /// A random desktop: 0 to 5 files of each kind plus 0 to 3 folders.
    fn random_model(rng: &mut Rng) -> Model {
        let (mut files, mut groups, mut loose_tail) = (Vec::new(), Vec::new(), Vec::new());
        for k in KIND_ORDER {
            let mut idx = Vec::new();
            for j in 0..rng.below(6) {
                idx.push(files.len());
                files.push(file(format!("C:\\d\\{:?}{}.x", k, j), false, k));
            }
            if !idx.is_empty() {
                groups.push((k, idx));
            }
        }
        for j in 0..rng.below(4) {
            loose_tail.push(files.len());
            files.push(file(format!("C:\\d\\folder{}", j), true, Kind::Other));
        }
        Model { files, groups, loose_tail }
    }

    #[test]
    fn every_file_is_shown_exactly_when_it_should_be() {
        let mut rng = Rng(7);
        for _ in 0..500 {
            let m = random_model(&mut rng);
            let expanded: HashSet<Kind> = KIND_ORDER.into_iter().filter(|_| rng.below(2) == 1).collect();
            let entries = build_entries(&m, &expanded);
            let keys: Vec<&String> = entries.iter().map(|e| &e.key).collect();
            let unique: HashSet<&String> = keys.iter().copied().collect();
            assert_eq!(keys.len(), unique.len(), "an entry appears twice");
            for (k, idx) in &m.groups {
                let stack_shown = unique.contains(&stack_key(*k));
                assert_eq!(stack_shown, idx.len() > 1, "a stack exists exactly for kinds with 2+ files");
                for &i in idx {
                    let shown = unique.contains(&file_key(&m.files[i].path));
                    assert_eq!(shown, idx.len() == 1 || expanded.contains(k), "file visibility follows its stack");
                }
            }
            for &i in &m.loose_tail {
                assert!(unique.contains(&file_key(&m.files[i].path)), "folders are always shown");
            }
            // membership lists exactly the files that live in a real stack
            let members = stack_membership(&m);
            let stacked: usize = m.groups.iter().filter(|(_, v)| v.len() > 1).map(|(_, v)| v.len()).sum();
            assert_eq!(members.len(), stacked);
        }
    }

    #[test]
    fn cells_never_overlap_and_every_cell_can_be_scrolled_into_view() {
        let mut rng = Rng(11);
        for _ in 0..300 {
            let (w, h) = (400.0 + rng.below(7600) as f32, 300.0 + rng.below(4200) as f32);
            let scale = 1.0 + rng.below(5) as f32 * 0.25;
            let n = 1 + rng.below(1500);
            let mut g = Geom::new(w, h, scale);
            if w < g.cell_w + 2.0 * g.margin_x {
                continue;
            }
            let mut seen = HashSet::new();
            for i in 0..n {
                let (x, y) = g.cell_pos(i);
                assert!(seen.insert(((x * 4.0) as i64, (y * 4.0) as i64)), "two icons in the same place");
                assert!(y >= 0.0 && y + g.cell_h <= h + 0.5, "an icon is cut off vertically");
            }
            let max = g.max_scroll(n);
            for i in [0, n / 2, n - 1] {
                g.scroll = 0.0;
                let (x0, _) = g.cell_pos(i);
                g.scroll = (g.margin_x - x0).clamp(0.0, max);
                let (x, _) = g.cell_pos(i);
                assert!(x >= -0.5 && x + g.cell_w <= w + 0.5, "icon {i} cannot be brought on screen");
            }
            g.scroll = 0.0;
            if g.cell_pos(n - 1).0 >= g.margin_x - 0.5 {
                assert!(max < 0.5, "nothing to scroll when everything fits");
            }
        }
    }

    #[test]
    fn animation_always_ends_exactly_on_the_final_layout() {
        let mut rng = Rng(13);
        let g = Geom::new(1920.0, 1080.0, 1.0);
        for _ in 0..200 {
            let m = random_model(&mut rng);
            let members = stack_membership(&m);
            let closed = build_entries(&m, &HashSet::new());
            let open = build_entries(&m, &KIND_ORDER.into_iter().collect());
            let mut a = Animator::new();
            a.retarget(&g, &[], &closed, &members, false);
            a.retarget(&g, &closed, &open, &members, true); // expand everything
            a.retarget(&g, &open, &closed, &members, true); // collapse again mid-flight
            a.finish();
            assert!(!a.active && a.ghosts.is_empty());
            assert_eq!(a.cur.len(), closed.len(), "only the final entries remain");
            for (i, e) in closed.iter().enumerate() {
                let (x, y) = g.cell_pos(i);
                let v = a.cur[&e.key];
                assert!((v.x - x).abs() < 0.01 && (v.y - y).abs() < 0.01 && (v.a - 1.0).abs() < 0.01 && (v.sc - 1.0).abs() < 0.01);
            }
        }
    }

    #[test]
    fn left_side_is_the_exact_mirror_of_the_right_side() {
        let mut rng = Rng(0xA11CE);
        for _ in 0..300 {
            let w = 400.0 + rng.below(3600) as f32;
            let h = 330.0 + rng.below(2600) as f32;
            let scale = 1.0 + rng.below(20) as f32 / 10.0;
            let size = [0.8f32, 1.0, 1.25][rng.below(3)];
            let mut r = Geom::with_options(w, h, scale, size, false);
            let mut l = Geom::with_options(w, h, scale, size, true);
            assert_eq!(r.rows, l.rows);
            let n = 1 + rng.below(400);
            let max = r.max_scroll(n);
            assert_eq!(max, l.max_scroll(n));
            let scroll = if max > 0.0 { max * rng.below(101) as f32 / 100.0 } else { 0.0 };
            r.scroll = scroll;
            l.scroll = scroll;
            let i = rng.below(n);
            let (rx, ry) = r.cell_pos(i);
            let (lx, ly) = l.cell_pos(i);
            assert_eq!(ry, ly);
            // the cell's left edge on one side mirrors the cell's right edge on the other
            assert!((lx + rx + r.cell_w - w).abs() < 0.01, "w={w} i={i} lx={lx} rx={rx}");
        }
    }

    #[test]
    fn on_the_left_side_every_cell_can_be_scrolled_fully_into_view() {
        let mut rng = Rng(77);
        for _ in 0..300 {
            let w = 400.0 + rng.below(3600) as f32;
            let h = 330.0 + rng.below(2600) as f32;
            let mut g = Geom::with_options(w, h, 1.0 + rng.below(20) as f32 / 10.0, [0.8f32, 1.0, 1.25][rng.below(3)], true);
            let n = 1 + rng.below(400);
            // first cell is fully visible with no scroll, last cell with full scroll
            let (x0, _) = g.cell_pos(0);
            assert!(x0 >= 0.0 && x0 + g.cell_w <= w + 0.01);
            g.scroll = g.max_scroll(n);
            let (x, _) = g.cell_pos(n - 1);
            assert!(x >= -0.01 && x + g.cell_w <= w + 0.01, "w={w} n={n} x={x}");
        }
    }

    #[test]
    fn icon_size_scales_cells_and_odd_sizes_fall_back_to_safe_values() {
        let m = Geom::with_options(1920.0, 1080.0, 1.0, 1.0, false);
        let s = Geom::with_options(1920.0, 1080.0, 1.0, 0.8, false);
        let l = Geom::with_options(1920.0, 1080.0, 1.0, 1.25, false);
        assert!(s.cell_w < m.cell_w && m.cell_w < l.cell_w);
        assert!(s.rows >= m.rows && m.rows >= l.rows && l.rows >= 1);
        assert_eq!(Geom::with_options(1920.0, 1080.0, 1.0, f32::NAN, false).s, m.s);
        assert_eq!(Geom::with_options(1920.0, 1080.0, 1.0, 0.0, false).s, 0.5);
        assert_eq!(Geom::with_options(1920.0, 1080.0, 1.0, 99.0, false).s, 2.0);
        assert_eq!(Geom::new(1920.0, 1080.0, 1.0).s, m.s);
    }

    #[test]
    fn one_wheel_notch_flips_exactly_one_file() {
        // three files: every one of them can be reached, in order (the bug was 1 -> 3 -> 1)
        let mut acc = 0;
        let a = scrub_step(None, 3, &mut acc, -120);
        assert_eq!(a, Some(1));
        let b = scrub_step(a, 3, &mut acc, -120);
        assert_eq!(b, Some(2));
        assert_eq!(scrub_step(b, 3, &mut acc, -120), Some(2)); // stays on the last file
        assert_eq!(scrub_step(b, 3, &mut acc, 120), Some(1));
        assert_eq!(scrub_step(Some(1), 3, &mut acc, 120), Some(0));
        assert_eq!(scrub_step(Some(0), 3, &mut acc, 120), Some(0)); // stays on the first file
        assert_eq!(scrub_step(None, 3, &mut acc, 120), Some(0));
    }

    #[test]
    fn small_touchpad_moves_add_up_to_one_file() {
        let mut acc = 0;
        assert_eq!(scrub_step(None, 5, &mut acc, -30), None);
        assert_eq!(scrub_step(None, 5, &mut acc, -30), None);
        assert_eq!(scrub_step(None, 5, &mut acc, -30), None);
        assert_eq!(scrub_step(None, 5, &mut acc, -30), Some(1));
        assert_eq!(acc, 0);
        // a fast flick of several notches at once
        assert_eq!(scrub_step(Some(1), 5, &mut acc, -360), Some(4));
    }

    #[test]
    fn scrubbing_never_leaves_the_stack_whatever_the_input() {
        let mut rng = Rng(2024);
        for _ in 0..500 {
            let len = rng.below(6);
            let mut acc = 0;
            let mut cur = if rng.below(2) == 0 { None } else { Some(rng.below(9)) };
            for _ in 0..20 {
                let delta = [-360, -120, -40, -1, 0, 1, 40, 120, 360, i32::MAX, i32::MIN][rng.below(11)];
                if let Some(n) = scrub_step(cur, len, &mut acc, delta) {
                    assert!(n < len, "len={len} n={n}");
                    cur = Some(n);
                }
            }
        }
    }
}
