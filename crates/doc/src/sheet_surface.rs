//! The flat of a sheet made from surfaces (#6748). The sheet, its panels on their surfaces where
//! they lie folded and the joints between them, is the master; its flat is derived from it by
//! `solidcraft_develop::develop`. Here are the pieces the development lays out (each placed panel
//! less its trims, the bend zones, and the tabs cuts leave), and the planar booleans they are cut
//! and joined by. The flat pattern is made of these pieces, and the folded solid of the same
//! pieces carried back onto the panels' surfaces, edge for edge.

use solidcore_record::{Loop2, Region2, Seg2, Vec2};
use solidcraft_develop::{Development, crossings};
use solidcraft_geom::prelude::*;
use solidcraft_geom::{Aabb2, Ring};
use solidcraft_kernel::BoolOp;

use crate::sheet::FlatWork;
use crate::sheet_surface_cuts::{holds, near_each_other, region_holds};
use crate::{DocError, Result};

/// Entry `index` of `list`, the `what` (say "face") an id read from a record or from a sheet's
/// own lists names: [`DocError::Missing`] where the list has no such entry, never a panic.
pub(crate) fn entry<'a, T>(list: &'a [T], index: usize, what: &'static str) -> Result<&'a T> {
    list.get(index).ok_or(DocError::Missing { what, index, count: list.len() })
}

/// A piece of a development's flat: a panel's material less its trims, a bend's zone (by its
/// place in the development's zones), or a tab: a piece of a zone that the cuts leave joined to
/// one of its panels only, which rides with that panel, flat, where the rest of the zone bends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Piece {
    Panel(usize),
    Zone(usize),
    /// A piece of zone `.0`, riding with panel `.1`.
    Tab(usize, usize),
}

/// The converted body's actual boundary edge and its two incident source faces. These are
/// construction identities, never a geometric match to a generated wall.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct ConvertedEdge {
    pub edge: usize,
    pub faces: [usize; 2],
}

/// Complete source sets aligned with a region's actual segments. None means unknown;
/// a boolean that meets an unknown boundary cannot manufacture a partial known set.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RegionOrigins {
    pub outer: Vec<Option<Vec<ConvertedEdge>>>,
    pub holes: Vec<Vec<Option<Vec<ConvertedEdge>>>>,
}

impl RegionOrigins {
    pub(crate) fn unknown(r: &Region2) -> Self {
        Self { outer: vec![None; r.outer.segs.len()], holes: r.holes.iter().map(|h| vec![None; h.segs.len()]).collect() }
    }

    pub(crate) fn loop_at(&self, k: usize) -> Option<&[Option<Vec<ConvertedEdge>>]> {
        if k == 0 { Some(&self.outer) } else { self.holes.get(k - 1).map(Vec::as_slice) }
    }

    /// Permute only metadata at a caller's existing loop-orientation event.
    pub(crate) fn oriented(&self, outer_reversed: bool, holes_reversed: &[bool]) -> Self {
        let mut out = self.clone();
        if outer_reversed {
            out.outer.reverse();
        }
        for (i, hole) in out.holes.iter_mut().enumerate() {
            if holes_reversed.get(i).copied().unwrap_or(false) {
                hole.reverse();
            }
        }
        out
    }

    /// Follow a development's own recorded parent segment or the source of a generated rip.
    fn developed(flat: &solidcraft_develop::FlatPanel, own: Option<&Self>, rips: &[Option<ConvertedEdge>], work: &mut FlatWork) -> Self {
        let cost = (|| {
            let mut count = 0usize;
            for lp in std::iter::once(&flat.region.outer).chain(&flat.region.holes) {
                count = count.checked_add(lp.segs.len())?;
            }
            let slots = count.checked_mul(std::mem::size_of::<Option<Vec<ConvertedEdge>>>() + std::mem::size_of::<ConvertedEdge>())?;
            slots.checked_add((flat.region.holes.len() + 1).checked_mul(std::mem::size_of::<Vec<Option<Vec<ConvertedEdge>>>>())?)
        })();
        if !work.charge_history(cost) {
            return Self::default();
        }
        let mut loops = std::iter::once(&flat.region.outer).chain(&flat.region.holes).enumerate().map(|(li, lp)| {
            lp.segs
                .iter()
                .enumerate()
                .map(|(si, _)| match flat.origins.get(li).and_then(|l| l.get(si)) {
                    Some(solidcraft_develop::EdgeFrom::Own(k)) => {
                        split_sources(own.and_then(|o| o.loop_at(li)).and_then(|l| l.get(*k)).unwrap_or(&None))
                    }
                    Some(solidcraft_develop::EdgeFrom::Rip(j)) => rips.get(*j).copied().flatten().map(|e| vec![e]),
                    None => None,
                })
                .collect::<Vec<_>>()
        });
        Self { outer: loops.next().unwrap_or_default(), holes: loops.collect() }
    }
}

/// Without parameter intervals, a split child's complete contributor set is known only
/// when its parent had one source. Mixed parents remain ambiguous, never a favorite subset.
pub(crate) fn split_sources(tag: &Option<Vec<ConvertedEdge>>) -> Option<Vec<ConvertedEdge>> {
    match tag.as_deref() {
        Some([one]) => Some(vec![*one]),
        _ => None,
    }
}

/// Precharge source-reference copies, Vec storage, and comparison work before allocation.
/// This observer has no influence on the geometry work allowance or its errors.
pub(crate) fn charge_sources<'a>(
    work: &mut FlatWork,
    tags: impl IntoIterator<Item = &'a Option<Vec<ConvertedEdge>>>,
    copies: usize,
    sorting: bool,
) -> bool {
    let cost = (|| {
        let mut slots = 0usize;
        let mut refs = 0usize;
        for tag in tags {
            slots = slots.checked_add(1)?;
            refs = refs.checked_add(tag.as_ref().map_or(0, Vec::len))?;
        }
        let storage = slots
            .checked_mul(std::mem::size_of::<Option<Vec<ConvertedEdge>>>())?
            .checked_add(refs.checked_mul(std::mem::size_of::<ConvertedEdge>())?)?
            .checked_mul(copies)?;
        let comparisons = if sorting { refs.checked_mul(refs.max(1).ilog2() as usize + 1)? } else { 0 };
        storage.checked_add(comparisons)
    })();
    work.charge_history(cost)
}

/// All known parents of one constructed run; one unknown segment makes the run unknown.
pub(crate) fn union_sources<'a, I>(tags: I, work: &mut FlatWork) -> Option<Vec<ConvertedEdge>>
where
    I: IntoIterator<Item = &'a Option<Vec<ConvertedEdge>>>,
    I::IntoIter: Clone,
{
    let tags = tags.into_iter();
    if !charge_sources(work, tags.clone(), 1, true) {
        return None;
    }
    let mut all = Vec::new();
    for tag in tags {
        all.extend(tag.as_ref().filter(|s| !s.is_empty())?.iter().copied());
    }
    all.sort();
    all.dedup();
    (!all.is_empty()).then_some(all)
}

/// An explicit paired result. Geometry-only callers deliberately discard origins.
#[derive(Clone, Debug)]
pub(crate) struct Regions<T = Region2> {
    pub geometry: Vec<T>,
    pub origins: Vec<RegionOrigins>,
}

impl Regions {
    pub(crate) fn unknown(geometry: Vec<Region2>) -> Self {
        let origins = geometry.iter().map(RegionOrigins::unknown).collect();
        Self { geometry, origins }
    }
}

pub(crate) type Pieces = Regions<(Piece, Region2)>;

#[derive(Clone, Copy)]
pub(crate) struct PieceSource<'a> {
    pub geometry: &'a [(Piece, Region2)],
    pub origins: &'a [RegionOrigins],
}

impl Pieces {
    pub(crate) fn source(&self) -> PieceSource<'_> {
        PieceSource { geometry: &self.geometry, origins: &self.origins }
    }
}

/// Resolve the kernel's current-input boundary indices against complete caller source sets.
pub(crate) fn compose_regions(
    output: Vec<solidcraft_kernel::RegionWithHistory>,
    a: &[RegionOrigins],
    b: &[RegionOrigins],
    work: &mut FlatWork,
) -> Regions {
    let overhead = (|| {
        let mut slots = 0usize;
        let mut loops = 0usize;
        for out in &output {
            loops = loops.checked_add(out.holes.len().checked_add(1)?)?;
            slots = slots.checked_add(out.outer.len())?;
            for hole in &out.holes {
                slots = slots.checked_add(hole.len())?;
            }
        }
        slots
            .checked_mul(std::mem::size_of::<Option<Vec<ConvertedEdge>>>())?
            .checked_add(loops.checked_mul(std::mem::size_of::<Vec<Option<Vec<ConvertedEdge>>>>())?)
    })();
    if !work.charge_history(overhead) {
        let geometry = output.into_iter().map(|out| out.region).collect::<Vec<_>>();
        let origins = vec![RegionOrigins::default(); geometry.len()];
        return Regions { geometry, origins };
    }
    let mut geometry = Vec::with_capacity(output.len());
    let mut origins = Vec::with_capacity(output.len());
    for out in output {
        let mut compose = |source: &Option<Vec<solidcraft_kernel::InputBoundary>>| -> Option<Vec<ConvertedEdge>> {
            let source = source.as_ref().filter(|s| !s.is_empty())?;
            let lookup = |s: &solidcraft_kernel::InputBoundary| -> Option<&Option<Vec<ConvertedEdge>>> {
                let input = match s.operand {
                    0 => a,
                    1 => b,
                    _ => return None,
                };
                let region = input.get(s.region)?;
                let lp = match s.loop_kind {
                    solidcraft_kernel::BoundaryLoop::Outer => &region.outer,
                    solidcraft_kernel::BoundaryLoop::Hole(k) => region.holes.get(k)?,
                };
                lp.get(s.segment)
            };
            // Resolve every parent before allocation; never retain a partial known set.
            let mut refs = 0usize;
            for s in source {
                let parents = lookup(s)?.as_ref().filter(|s| !s.is_empty())?;
                if !s.whole && parents.len() != 1 {
                    return None;
                }
                refs = refs.checked_add(parents.len())?;
            }
            let cost = refs
                .checked_mul(std::mem::size_of::<ConvertedEdge>())
                .and_then(|n| n.checked_add(std::mem::size_of::<Vec<ConvertedEdge>>()))
                .and_then(|n| refs.checked_mul(refs.max(1).ilog2() as usize + 1).and_then(|sort| n.checked_add(sort)));
            if !work.charge_history(cost) {
                return None;
            }
            let mut all = Vec::new();
            for s in source {
                let input = match s.operand {
                    0 => a,
                    1 => b,
                    _ => return None,
                };
                let region = input.get(s.region)?;
                let lp = match s.loop_kind {
                    solidcraft_kernel::BoundaryLoop::Outer => &region.outer,
                    solidcraft_kernel::BoundaryLoop::Hole(k) => region.holes.get(k)?,
                };
                all.extend(lp.get(s.segment)?.as_ref().filter(|s| !s.is_empty())?.iter().copied());
            }
            all.sort();
            all.dedup();
            (!all.is_empty()).then_some(all)
        };
        origins.push(RegionOrigins {
            outer: out.outer.iter().map(&mut compose).collect(),
            holes: out.holes.iter().map(|h| h.iter().map(&mut compose).collect()).collect(),
        });
        geometry.push(out.region);
    }
    Regions { geometry, origins }
}

/// How near (mm) a cut may come to a piece's loops and still be a hole of it as it is: as near
/// as the sew joins points.
const CLEAR: f64 = 1e-6;

/// How far (mm) a piece's edge may lie off a zone's tangent line and still run along it.
const ON_LINE: f64 = 1e-7;

/// A development's pieces before they are joined: each placed panel less its trims, and the
/// zones, all less what the development cuts from the flat (the corners' reliefs and the sheet's
/// cuts), with each piece of a zone sorted by what still holds it (see [`sort_zones`]), and their
/// longest arcs halved (see [`halved`]). The flat pattern and the folded solid are both made of
/// them, edge for edge.
pub(crate) fn pieces(dev: &Development, seeds: &[RegionOrigins], rips: &[Option<ConvertedEdge>], work: &mut FlatWork) -> Result<Pieces> {
    #[cfg(test)]
    crate::eval::cost_tests::count_flat();
    let mut pieces = Vec::new();
    let mut origins = Vec::new();
    for (i, flat) in dev.panels.iter().enumerate() {
        let Some(flat) = flat else { continue };
        let placed = on_grid(&flat.region);
        let source = RegionOrigins::developed(flat, seeds.get(i), rips, work);
        let trims: Vec<Region2> = dev.trims.iter().filter(|t| t.panel == i).map(|t| on_grid(&t.region)).collect();
        if trims.is_empty() {
            pieces.push((Piece::Panel(i), placed));
            origins.push(source);
        } else {
            // Until the kernel's planar booleans take several tools that share an edge (#6748):
            // trims of two bends can share an edge and a corner with the panel, which the
            // planar booleans take apart only one tool at a time, so join them first.
            let trims = stage(&format!("joining panel {i}'s trims"), planar_union(Regions::unknown(trims), work))?;
            let left =
                stage(&format!("trimming panel {i}"), boolean_past_holes(&[placed], &trims.geometry, BoolOp::Cut, &[source], &trims.origins, work))?;
            pieces.extend(left.geometry.into_iter().map(|r| (Piece::Panel(i), r)));
            origins.extend(left.origins);
        }
    }
    pieces.extend(dev.zones.iter().enumerate().map(|(z, zone)| (Piece::Zone(z), on_grid(&zone.region))));
    origins.extend(dev.zones.iter().map(|zone| RegionOrigins::unknown(&zone.region)));
    let pieces = Regions { geometry: pieces, origins };
    if dev.cuts.is_empty() {
        return Ok(halved(pieces, work));
    }
    // A cut clear inside one piece is a hole of it as it is, its curves kept; the rest are cut
    // from every piece they reach. A cut that came clear of the pieces but meets what the others
    // cut from them is cut the same way, after them.
    let cuts: Vec<Region2> = dev.cuts.iter().map(on_grid).collect();
    let (inside, across): (Vec<&Region2>, Vec<&Region2>) = cuts.iter().partition(|c| pieces.geometry.iter().any(|(_, r)| holds_clear(r, c)));
    let mut out = cut_from(pieces, &across, work)?;
    let mut late = Vec::new();
    for c in inside {
        match out.geometry.iter_mut().zip(&mut out.origins).find(|((_, r), _)| holds_clear(r, c)) {
            Some(((_, r), source)) => {
                source.holes.push(vec![None; c.outer.segs.len()]);
                r.holes.push(c.outer.ccw().reversed());
            }
            None => late.push(c),
        }
    }
    Ok(halved(sort_zones(dev, cut_from(out, &late, work)?)?, work))
}

/// The pieces with each arc of more than three quarters of a turn (a hole's whole circle) halved
/// at the middle of its own sweep: two arcs of its circle that meet where the one did. Until the
/// kernel (solidcore, #1597) takes a circular edge round any sweep, and a whole circle on one
/// vertex, the folded solid's wall along such an arc is two faces, one on each half (see
/// `sheet_surface_fold::cells`, which takes these pieces' arcs as they are). The flat keeps the
/// same two edges: an edge of the flat is an edge of the folded sheet.
fn halved(mut pieces: Pieces, work: &mut FlatWork) -> Pieces {
    let tags = pieces.origins.iter().flat_map(|r| r.outer.iter().chain(r.holes.iter().flatten()));
    if !charge_sources(work, tags, 4, false) {
        pieces.origins = vec![RegionOrigins::default(); pieces.geometry.len()];
    }
    let halves = |l: Loop2, source: Vec<Option<Vec<ConvertedEdge>>>| {
        let mut tags = Vec::new();
        let segs = l
            .segs
            .into_iter()
            .enumerate()
            .flat_map(|(k, e)| {
                let tag = source.get(k).cloned().flatten();
                match e {
                    Seg2::Arc { center, radius, start, sweep } if sweep.abs() > std::f64::consts::PI * 1.5 => {
                        let half = sweep / 2.0;
                        let tag = split_sources(&tag);
                        tags.extend([tag.clone(), tag]);
                        vec![Seg2::Arc { center, radius, start, sweep: half }, Seg2::Arc { center, radius, start: start + half, sweep: half }]
                    }
                    e => {
                        tags.push(tag);
                        vec![e]
                    }
                }
            })
            .collect();
        (Loop2 { segs }, tags)
    };
    let mut geometry = Vec::new();
    let mut origins = Vec::new();
    for (k, (piece, r)) in pieces.geometry.into_iter().enumerate() {
        let source = pieces.origins.get(k).cloned().unwrap_or_else(|| RegionOrigins::unknown(&r));
        let (outer, outer_tags) = halves(r.outer, source.outer);
        let mut holes = Vec::new();
        let mut hole_tags = Vec::new();
        for (i, h) in r.holes.into_iter().enumerate() {
            let (h, tags) = halves(h, source.holes.get(i).cloned().unwrap_or_default());
            holes.push(h);
            hole_tags.push(tags);
        }
        geometry.push((piece, Region2 { outer, holes }));
        origins.push(RegionOrigins { outer: outer_tags, holes: hole_tags });
    }
    Regions { geometry, origins }
}

/// `pieces` less `cuts`, joined first: what each piece leaves, each a piece of the same kind.
fn cut_from(mut pieces: Pieces, cuts: &[&Region2], work: &mut FlatWork) -> Result<Pieces> {
    if !charge_sources(work, pieces.origins.iter().flat_map(|r| r.outer.iter().chain(r.holes.iter().flatten())), 1, false) {
        pieces.origins = vec![RegionOrigins::default(); pieces.geometry.len()];
    }
    if cuts.is_empty() {
        return Ok(pieces);
    }
    let cuts = stage("joining what is cut from the flat", planar_union(Regions::unknown(cuts.iter().map(|c| (*c).clone()).collect()), work))?;
    let mut out = Vec::with_capacity(pieces.geometry.len());
    let mut origins = Vec::new();
    for (k, (piece, region)) in pieces.geometry.into_iter().enumerate() {
        let source = pieces.origins.get(k).cloned().unwrap_or_else(|| RegionOrigins::unknown(&region));
        let cut = stage("cutting the flat", boolean_past_holes(&[region], &cuts.geometry, BoolOp::Cut, &[source], &cuts.origins, work))?;
        out.extend(cut.geometry.into_iter().map(|r| (piece, r)));
        origins.extend(cut.origins);
    }
    Ok(Regions { geometry: out, origins })
}

/// Whether `cut` lies clear inside `region`, so that it is a hole of it as it is: within its
/// outline, outside its holes, crossing none of its loops and nowhere nearer them than
/// [`CLEAR`].
pub(crate) fn holds_clear(region: &Region2, cut: &Region2) -> bool {
    let Some(start) = cut.outer.segs.first().map(Seg2::start) else { return false };
    if !cut.holes.is_empty() || !region_holds(region, start) {
        return false;
    }
    // Chords this far inside the curves: two loops whose chords stay farther apart than twice
    // that and `CLEAR` are clear of each other.
    let sag: f64 = 1e-4;
    let ring = std::cell::OnceCell::new();
    // Exact upper bound on the chord deviation of `Seg2::polyline(sag)` for lines and arcs;
    // `None` for other curve kinds or degenerate arcs, which fall back to the polyline check.
    let seg_sag = |s: &Seg2| -> Option<f64> {
        match *s {
            Seg2::Line { .. } => Some(0.0),
            Seg2::Arc { radius, sweep, .. } => {
                if !(radius.is_finite() && radius > 0.0 && sweep.is_finite()) {
                    return None;
                }
                let tol = sag.max(radius.abs() * 1e-6).max(1e-9);
                let step = if tol < radius { 2.0 * (1.0 - tol / radius).clamp(-1.0, 1.0).acos() } else { std::f64::consts::TAU / 8.0 };
                let n = ((sweep.abs() / step).ceil() as usize).clamp(2, 4096);
                let half = sweep.abs() / (2.0 * n as f64);
                if half > std::f64::consts::FRAC_PI_2 {
                    return None;
                }
                Some(radius * (1.0 - half.cos()))
            }
            _ => None,
        }
    };
    let loop_joint_gap = |l: &Loop2| l.joints().map(|(a, b)| a.end().dist(b.start())).fold(0.0_f64, f64::max);
    let cut_max_sag = cut.outer.segs.iter().try_fold(0.0_f64, |m, s| Some(m.max(seg_sag(s)?)));
    let cut_gap = loop_joint_gap(&cut.outer);
    // Boxes that hold the loops' curves (see [`loop_box`]): a loop whose box stays farther than
    // `CLEAR` from the cut's is clear of it.
    let reach = loop_box(&cut.outer);
    let near = |b: &Aabb2| b.min.x <= reach.max.x && reach.min.x <= b.max.x && b.min.y <= reach.max.y && reach.min.y <= b.max.y;
    let gap = |a: &Ring<Vec2>, b: &Ring<Vec2>| {
        a.iter().map(|p| b.edges().map(|(q0, q1)| segment_distance(*p, *q0, *q1)).fold(f64::INFINITY, f64::min)).fold(f64::INFINITY, f64::min)
    };
    std::iter::once(&region.outer).chain(&region.holes).all(|l| {
        if !near(&crate::sheet_surface_cuts::loop_box(l)) {
            return true;
        }
        if l.segs.first().is_some_and(|s| holds(&cut.outer, s.start())) {
            return false;
        }
        if let (Some(c_sag), Some(l_sag)) = (cut_max_sag, l.segs.iter().try_fold(0.0_f64, |m, s| Some(m.max(seg_sag(s)?)))) {
            let joint_gap = cut_gap.max(loop_joint_gap(l));
            if joint_gap.is_finite() {
                let margin = 2.0 * sag + CLEAR + c_sag.max(l_sag) + joint_gap + 1e-12;
                if cut.outer.segs.iter().all(|s| l.segs.iter().all(|e| matches!(near_each_other(s, e, margin), Ok(false)))) {
                    return true;
                }
            }
        }
        if !crossings(&cut.outer.segs, &l.segs).is_empty() {
            return false;
        }
        let ring = ring.get_or_init(|| cut.outer.polyline(sag));
        let other = l.polyline(sag);
        gap(ring, &other).min(gap(&other, ring)) > 2.0 * sag + CLEAR
    })
}

/// How far `p` lies from the segment from `a` to `b`.
fn segment_distance(p: Vec2, a: Vec2, b: Vec2) -> f64 {
    let d = b - a;
    let t = if d.len2() > 0.0 { ((p - a).dot(d) / d.len2()).clamp(0.0, 1.0) } else { 0.0 };
    p.dist(a + d * t)
}

/// The pieces with each piece of a zone sorted by what still holds it: one that runs along both
/// its panels' tangent lines bends with the zone; one that runs along only one is a tab of that
/// panel. A piece that runs along neither is refused. A zone left with nothing that bends (cuts
/// part the sheet there) leaves its panels as pieces of their own, each where the bend held it.
fn sort_zones(dev: &Development, pieces: Pieces) -> Result<Pieces> {
    let mut out = Vec::with_capacity(pieces.geometry.len());
    for (piece, region) in pieces.geometry {
        let Piece::Zone(z) = piece else {
            out.push((piece, region));
            continue;
        };
        let zone = dev.zones.get(z).ok_or_else(|| DocError::Invalid(format!("the flat has no bend {}", z + 1)))?;
        let sorted = match (along(&region, zone.near), along(&region, zone.far)) {
            (true, true) => Piece::Zone(z),
            (true, false) => Piece::Tab(z, zone.panels[0]),
            (false, true) => Piece::Tab(z, zone.panels[1]),
            (false, false) => {
                return Err(DocError::Invalid(format!("a cut leaves a piece of bend {} joined to neither of its panels: not supported yet", z + 1)));
            }
        };
        out.push((sorted, region));
    }
    Ok(Regions { geometry: out, origins: pieces.origins })
}

/// Whether an edge of `region`'s outline runs along the line from `a` to `b` for more than a
/// rounding.
pub(crate) fn along(region: &Region2, [a, b]: [Vec2; 2]) -> bool {
    let l = a.dist(b);
    let Some(u) = (b - a).normalized() else { return false };
    let on = |p: Vec2| (p - a).cross(u).abs() <= ON_LINE && (-ON_LINE..=l + ON_LINE).contains(&(p - a).dot(u));
    region.outer.segs.iter().any(|s| matches!(s, Seg2::Line { .. }) && s.length() > ON_LINE && on(s.start()) && on(s.end()))
}

/// A development's pieces (see [`pieces`]) joined into its flat.
pub(crate) fn join_pieces(pieces: &[(Piece, Region2)], work: &mut FlatWork) -> Result<Vec<Region2>> {
    let regions: Vec<Region2> = pieces.iter().map(|(_, r)| r.clone()).collect();
    // The holes clear of every other piece, and of their own piece's outline and other holes
    // (see [`holes_apart`]), stay out of the union, and go back after it. A hole that touches its
    // piece's outline (a cut-out with a corner on the sheet's edge, which the planar booleans
    // read so) is the outline there: it goes through the union with it.
    let reach: Vec<Aabb2> = regions.iter().map(|r| loop_box(&r.outer)).collect();
    let mut carried: Vec<Loop2> = Vec::new();
    let mut bare = Vec::with_capacity(regions.len());
    for (i, r) in regions.into_iter().enumerate() {
        let others: Vec<Aabb2> = reach.iter().enumerate().filter(|(k, _)| *k != i).map(|(_, b)| *b).collect();
        let apart = holes_apart(&r);
        let mut near = Vec::new();
        for (h, apart) in r.holes.into_iter().zip(apart) {
            if apart && boxes_clear(&loop_box(&h), &others) {
                carried.push(h);
            } else {
                near.push(h);
            }
        }
        bare.push(Region2 { outer: r.outer, holes: near });
    }
    let joined = stage("joining the sheet's pieces", planar_union(Regions::unknown(bare), work))?;
    let carried = carried
        .into_iter()
        .map(|h| {
            let tags = vec![None; h.segs.len()];
            (h, tags)
        })
        .collect();
    Ok(stage("joining the sheet's pieces", put_back(joined, carried, work))?.geometry)
}

/// A loop's box, out by [`CLEAR`]: the box that holds its segments (see
/// [`crate::sheet_surface_cuts::loop_box`]).
fn loop_box(l: &Loop2) -> Aabb2 {
    crate::sheet_surface_cuts::loop_box(l).inflated(CLEAR)
}

/// Whether box `b` meets none of `of`.
fn boxes_clear(b: &Aabb2, of: &[Aabb2]) -> bool {
    of.iter().all(|o| b.max.x < o.min.x || o.max.x < b.min.x || b.max.y < o.min.y || o.max.y < b.min.y)
}

/// Whether loops `h` and `l`, each with its box (see [`loop_box`]), keep farther apart than
/// [`CLEAR`]: their boxes apart, or no segment of one that near a segment of the other, read
/// exactly (see [`near_each_other`]). A pair of curves no exact reading is known for is taken as
/// near: its loop goes through the planar booleans, which read it exactly.
fn loops_apart((h, hb): (&Loop2, Aabb2), (l, lb): (&Loop2, Aabb2)) -> bool {
    boxes_clear(&hb, std::slice::from_ref(&lb)) || h.segs.iter().all(|s| l.segs.iter().all(|e| matches!(near_each_other(s, e, CLEAR), Ok(false))))
}

/// For each hole of `region`, whether it keeps clear of the region's outline and its other holes
/// (see [`loops_apart`]). A hole that touches one of them, at a point or along an edge, bounds the
/// region's material with it: the planar booleans read a cut-out with a corner on the outline as
/// a hole that meets the outline at that corner. Such a hole is not a hole of the region as it
/// is: a boolean of the region takes it with the outline, never past it (see [`put_back`]).
fn holes_apart(region: &Region2) -> Vec<bool> {
    let n = region.holes.len();
    if n == 0 {
        return Vec::new();
    }
    let outer = (&region.outer, loop_box(&region.outer));
    let hole_boxes: Vec<Aabb2> = region.holes.iter().map(loop_box).collect();
    let mut apart: Vec<bool> = region.holes.iter().zip(&hole_boxes).map(|(h, &hb)| loops_apart((h, hb), outer)).collect();
    // Sweep holes sorted by min.x: two holes whose inflated boxes overlap in x must satisfy
    // later.min.x <= earlier.max.x, so the inner loop breaks once min.x passes max.x.
    let mut finite: Vec<(usize, &Loop2, Aabb2)> = Vec::with_capacity(n);
    let mut other: Vec<(usize, &Loop2, Aabb2)> = Vec::new();
    for (i, (h, &hb)) in region.holes.iter().zip(&hole_boxes).enumerate() {
        if hb.min.x.is_finite() && hb.max.x.is_finite() {
            finite.push((i, h, hb));
        } else {
            other.push((i, h, hb));
        }
    }
    finite.sort_by(|a, b| a.2.min.x.total_cmp(&b.2.min.x));
    for (pos, &(i, hi, bi)) in finite.iter().enumerate() {
        for &(j, hj, bj) in finite.get(pos + 1..).unwrap_or(&[]) {
            if bj.min.x > bi.max.x {
                break;
            }
            if !apart.get(i).copied().unwrap_or(false) && !apart.get(j).copied().unwrap_or(false) {
                continue;
            }
            if bi.max.y < bj.min.y || bj.max.y < bi.min.y {
                continue;
            }
            if !loops_apart((hi, bi), (hj, bj)) {
                if let Some(slot) = apart.get_mut(i) {
                    *slot = false;
                }
                if let Some(slot) = apart.get_mut(j) {
                    *slot = false;
                }
            }
        }
    }
    for &(i, hi, bi) in &other {
        for (j, (hj, &bj)) in region.holes.iter().zip(&hole_boxes).enumerate() {
            if i == j || (!apart.get(i).copied().unwrap_or(false) && !apart.get(j).copied().unwrap_or(false)) {
                continue;
            }
            if !loops_apart((hi, bi), (hj, bj)) {
                if let Some(slot) = apart.get_mut(i) {
                    *slot = false;
                }
                if let Some(slot) = apart.get_mut(j) {
                    *slot = false;
                }
            }
        }
    }
    apart
}

/// The holes `carried` back in the regions of `out` that hold them: each kept clear of every
/// other loop (see [`holes_apart`] and [`loop_clear_of`]), so that the region holding a point of
/// it holds it all, read exactly (see [`holds`]).
fn put_back(mut out: Regions, carried: Vec<(Loop2, Vec<Option<Vec<ConvertedEdge>>>)>, work: &mut FlatWork) -> Result<Regions> {
    let mut boxes: Vec<(Aabb2, Vec<Aabb2>)> = out.geometry.iter().map(|r| (loop_box(&r.outer), r.holes.iter().map(loop_box).collect())).collect();
    for (h, source) in carried {
        let Some(at) = h.segs.first().map(Seg2::start) else { continue };
        let ((r, tags), (_, hole_boxes)) = out
            .geometry
            .iter_mut()
            .zip(&mut out.origins)
            .zip(&mut boxes)
            .find(|((r, _), (outer_box, hole_boxes))| {
                outer_box.contains(at) && holds(&r.outer, at) && !r.holes.iter().zip(hole_boxes.iter()).any(|(k, kb)| kb.contains(at) && holds(k, at))
            })
            .ok_or_else(|| DocError::Invalid("a cut-out of the flat lost its piece".into()))?;
        hole_boxes.push(loop_box(&h));
        let hole_index = r.holes.len();
        r.holes.push(h);
        let needed = hole_index.checked_add(1);
        let storage = needed.and_then(|n| n.checked_mul(std::mem::size_of::<Vec<Option<Vec<ConvertedEdge>>>>())).and_then(|n| n.checked_mul(2));
        let additional = needed.map(|n| n.saturating_sub(tags.holes.len()));
        if work.charge_history(storage) && additional.is_some_and(|n| tags.holes.try_reserve_exact(n).is_ok()) {
            tags.holes.resize_with(hole_index, Vec::new);
            tags.holes.push(source);
        } else {
            *tags = RegionOrigins::default();
        }
    }
    Ok(out)
}

/// Whether loop `h` keeps clear of region `t`: apart from it by their boxes, or farther than
/// [`CLEAR`] from each of its loops (see [`loops_apart`]), outside its material and holding none
/// of it. Apart from every loop of `t`, `h` lies all inside or all outside each, and each of them
/// all inside or all outside `h`: one point of each answers, exactly (see [`holds`]).
pub(crate) fn loop_clear_of(h: &Loop2, t: &Region2) -> bool {
    let hb = loop_box(h);
    if boxes_clear(&hb, std::slice::from_ref(&loop_box(&t.outer))) {
        return true;
    }
    let loops: Vec<&Loop2> = std::iter::once(&t.outer).chain(&t.holes).collect();
    if !loops.iter().all(|l| loops_apart((h, hb), (*l, loop_box(l)))) {
        return false;
    }
    let Some(p) = h.segs.first().map(Seg2::start) else { return false };
    let within = holds(&t.outer, p) && !t.holes.iter().any(|k| holds(k, p));
    !within && !loops.iter().any(|l| l.segs.first().is_some_and(|s| holds(h, s.start())))
}

/// `op` of `a` with `b` ([`planar_boolean`]), the holes of `a` clear of every region of `b`
/// (see [`loop_clear_of`]) and of their own region's other loops (see [`holes_apart`]) left out
/// of it and put back in the region of the result that holds them.
pub(crate) fn boolean_past_holes(
    a: &[Region2],
    b: &[Region2],
    op: BoolOp,
    a_origins: &[RegionOrigins],
    b_origins: &[RegionOrigins],
    work: &mut FlatWork,
) -> Result<Regions> {
    let clear_of_tools = |h: &Loop2| b.iter().all(|t| loop_clear_of(h, t));
    let a_origins =
        if charge_sources(work, a_origins.iter().flat_map(|r| r.outer.iter().chain(r.holes.iter().flatten())), 2, false) { a_origins } else { &[] };
    let mut carried = Vec::new();
    let mut bare_origins = Vec::new();
    let bare: Vec<Region2> = a
        .iter()
        .enumerate()
        .map(|(ri, r)| {
            let source = a_origins.get(ri).cloned().unwrap_or_else(|| RegionOrigins::unknown(r));
            let mut near = Vec::new();
            let mut near_origins = Vec::new();
            for (hi, (h, apart)) in r.holes.iter().zip(holes_apart(r)).enumerate() {
                let tags = source.holes.get(hi).cloned().unwrap_or_else(|| vec![None; h.segs.len()]);
                if apart && clear_of_tools(h) {
                    carried.push((h.clone(), tags));
                } else {
                    near.push(h.clone());
                    near_origins.push(tags);
                }
            }
            bare_origins.push(RegionOrigins { outer: source.outer, holes: near_origins });
            Region2 { outer: r.outer.clone(), holes: near }
        })
        .collect();
    if carried.is_empty() {
        return planar_boolean(a, b, op, a_origins, b_origins, work);
    }
    put_back(planar_boolean(&bare, b, op, &bare_origins, b_origins, work)?, carried, work)
}

/// How far apart (mm) the planar booleans let a loop's segments end and still close it: the
/// kernel's own reading of a loop, which moves nothing.
const CLOSE: f64 = 1e-7;

/// `op` of `a` with `b` by the kernel's planar booleans, as the regions are: every edge of the
/// result is a piece of an edge of `a` or `b`, however short, and no corner moves. The work is
/// charged to `work`, which bounds the flat's.
pub(crate) fn planar_boolean(
    a: &[Region2],
    b: &[Region2],
    op: BoolOp,
    a_origins: &[RegionOrigins],
    b_origins: &[RegionOrigins],
    work: &mut FlatWork,
) -> Result<Regions> {
    let edges =
        |rs: &[Region2]| rs.iter().flat_map(|r| std::iter::once(&r.outer).chain(&r.holes)).fold(0usize, |n, l| n.saturating_add(l.segs.len()));
    let n = edges(a).saturating_add(edges(b));
    work.charge(n.saturating_mul(n).saturating_mul(16))?;
    Ok(compose_regions(solidcraft_kernel::planar_regions_boolean_with_history(a, b, op, CLOSE)?, a_origins, b_origins, work))
}

/// The union of `regions` ([`planar_boolean`]), in balanced rounds: many pieces side by side do
/// not each meet a growing union one at a time.
pub(crate) fn planar_union(mut regions: Regions, work: &mut FlatWork) -> Result<Regions> {
    if !charge_sources(work, regions.origins.iter().flat_map(|r| r.outer.iter().chain(r.holes.iter().flatten())), 1, false) {
        regions.origins = vec![RegionOrigins::default(); regions.geometry.len()];
    }
    let mut groups: Vec<Regions> = regions
        .geometry
        .into_iter()
        .enumerate()
        .map(|(k, r)| {
            let source = regions.origins.get(k).cloned().unwrap_or_else(|| RegionOrigins::unknown(&r));
            Regions { geometry: vec![r], origins: vec![source] }
        })
        .collect();
    while groups.len() > 1 {
        let mut next = Vec::with_capacity(groups.len().div_ceil(2));
        let mut iter = groups.into_iter();
        while let Some(a) = iter.next() {
            match iter.next() {
                Some(b) => next.push(planar_boolean(&a.geometry, &b.geometry, BoolOp::Union, &a.origins, &b.origins, work)?),
                None => next.push(a),
            }
        }
        groups = next;
    }
    Ok(groups.pop().unwrap_or_else(|| Regions::unknown(Vec::new())))
}

/// A region with its points on a nanometer grid. Until the kernel's planar booleans weld edges
/// that nearly coincide (#6748): the development is exact, but two pieces can reach one corner by
/// different sums and land a rounding apart, and the planar booleans take two edges that nearly
/// coincide for two, where on the grid they coincide.
fn on_grid(r: &Region2) -> Region2 {
    let g = |v: Vec2| Vec2::new((v.x * 1e9).round() / 1e9, (v.y * 1e9).round() / 1e9);
    let seg = |s: &Seg2| match *s {
        Seg2::Line { a, b } => Seg2::Line { a: g(a), b: g(b) },
        Seg2::Arc { center, radius, start, sweep } => Seg2::Arc { center: g(center), radius, start, sweep },
        Seg2::Cubic { p0, p1, p2, p3 } => Seg2::Cubic { p0: g(p0), p1: g(p1), p2: g(p2), p3: g(p3) },
        Seg2::Conic { a, apex, b, w } => Seg2::Conic { a: g(a), apex: g(apex), b: g(b), w },
        Seg2::Bezier { n, mut p } => {
            for q in p.iter_mut().take(usize::from(n) + 1) {
                *q = g(*q);
            }
            Seg2::Bezier { n, p }
        }
    };
    let lp = |l: &Loop2| Loop2 { segs: l.segs.iter().map(seg).collect() };
    Region2 { outer: lp(&r.outer), holes: r.holes.iter().map(lp).collect() }
}

/// Names the step of making the flat an error came from.
fn stage<T>(what: &str, r: Result<T>) -> Result<T> {
    r.map_err(|e| DocError::Invalid(format!("{what}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect_loop(x0: f64, y0: f64, x1: f64, y1: f64) -> Loop2 {
        Loop2::polygon(&[Vec2::new(x0, y0), Vec2::new(x1, y0), Vec2::new(x1, y1), Vec2::new(x0, y1)])
    }

    #[test]
    fn exhausted_boundary_history_keeps_the_same_boolean_geometry() {
        let a = Region2 { outer: rect_loop(0.0, 0.0, 20.0, 10.0), holes: Vec::new() };
        let b = Region2 { outer: rect_loop(5.0, -5.0, 15.0, 15.0), holes: Vec::new() };
        let edge = ConvertedEdge { edge: 0, faces: [0, 1] };
        let origins = RegionOrigins { outer: vec![Some(vec![edge]); 4], holes: Vec::new() };
        let live = planar_boolean(
            std::slice::from_ref(&a),
            std::slice::from_ref(&b),
            BoolOp::Cut,
            std::slice::from_ref(&origins),
            &[],
            &mut FlatWork::new(),
        )
        .unwrap();
        assert!(live.origins.iter().flat_map(|r| &r.outer).any(Option::is_some));
        let mut exhausted = FlatWork::new();
        assert!(!exhausted.charge_history(None));
        let without_history = planar_boolean(&[a], &[b], BoolOp::Cut, &[origins], &[], &mut exhausted).unwrap();
        assert_eq!(without_history.geometry, live.geometry);
        assert_eq!(without_history.geometry.iter().map(Region2::area).sum::<f64>(), 100.0);
        assert!(without_history.origins.iter().flat_map(|r| r.outer.iter().chain(r.holes.iter().flatten())).all(Option::is_none));
        assert!(exhausted.charge(1).is_ok(), "optional history never spends the geometry budget");
    }

    fn ref_holds_clear(region: &Region2, cut: &Region2) -> bool {
        let Some(start) = cut.outer.segs.first().map(Seg2::start) else { return false };
        if !cut.holes.is_empty() || !region_holds(region, start) {
            return false;
        }
        let sag: f64 = 1e-4;
        let ring = cut.outer.polyline(sag);
        let reach = loop_box(&cut.outer);
        let near = |b: &Aabb2| b.min.x <= reach.max.x && reach.min.x <= b.max.x && b.min.y <= reach.max.y && reach.min.y <= b.max.y;
        let gap = |a: &Ring<Vec2>, b: &Ring<Vec2>| {
            a.iter().map(|p| b.edges().map(|(q0, q1)| segment_distance(*p, *q0, *q1)).fold(f64::INFINITY, f64::min)).fold(f64::INFINITY, f64::min)
        };
        std::iter::once(&region.outer).chain(&region.holes).all(|l| {
            let other = l.polyline(sag);
            !near(&crate::sheet_surface_cuts::loop_box(l))
                || (l.segs.first().is_none_or(|s| !holds(&cut.outer, s.start()))
                    && crossings(&cut.outer.segs, &l.segs).is_empty()
                    && gap(&ring, &other).min(gap(&other, &ring)) > 2.0 * sag + CLEAR)
        })
    }

    fn ref_holes_apart(region: &Region2) -> Vec<bool> {
        let loops: Vec<(&Loop2, Aabb2)> = std::iter::once(&region.outer).chain(&region.holes).map(|l| (l, loop_box(l))).collect();
        loops.iter().enumerate().skip(1).map(|(k, h)| loops.iter().enumerate().all(|(j, l)| j == k || loops_apart(*h, *l))).collect()
    }

    fn ref_put_back(mut out: Vec<Region2>, carried: Vec<Loop2>) -> Result<Vec<Region2>> {
        for h in carried {
            let Some(at) = h.segs.first().map(Seg2::start) else { continue };
            let r = out
                .iter_mut()
                .find(|r| holds(&r.outer, at) && !r.holes.iter().any(|k| holds(k, at)))
                .ok_or_else(|| DocError::Invalid("a cut-out of the flat lost its piece".into()))?;
            r.holes.push(h);
        }
        Ok(out)
    }

    #[test]
    fn holds_clear_matches_reference_across_many_holes_and_near_boundaries() {
        let mut holes = Vec::with_capacity(144);
        for row in 0..12 {
            for col in 0..12 {
                let c = Vec2::new(15.0 + col as f64 * 15.0, 15.0 + row as f64 * 15.0);
                holes.push(Loop2::circle(c, 3.0).reversed());
            }
        }
        let region = Region2 { outer: rect_loop(0.0, 0.0, 200.0, 200.0), holes };

        // Well-separated circle and polygon in an open slot between grid holes.
        let clear_circle = Region2 { outer: Loop2::circle(Vec2::new(22.5, 22.5), 2.5), holes: Vec::new() };
        let clear_rect = Region2 { outer: rect_loop(20.0, 20.0, 25.0, 25.0), holes: Vec::new() };
        assert!(holds_clear(&region, &clear_circle));
        assert_eq!(holds_clear(&region, &clear_circle), ref_holds_clear(&region, &clear_circle));
        assert!(holds_clear(&region, &clear_rect));
        assert_eq!(holds_clear(&region, &clear_rect), ref_holds_clear(&region, &clear_rect));

        // Near-boundary circles against both the outer edge (x = 0) and an existing hole at (15, 15), r = 3.
        for gap in [0.5 * CLEAR, 2.0 * CLEAR, 1.5e-4, 2.0e-4 + CLEAR, 2.5e-4, 3.5e-4, 5.0e-4, 1.0e-3] {
            let near_outer = Region2 { outer: Loop2::circle(Vec2::new(2.0 + gap, 7.5), 2.0), holes: Vec::new() };
            assert_eq!(holds_clear(&region, &near_outer), ref_holds_clear(&region, &near_outer), "outer gap={gap:e}");
            let near_hole = Region2 { outer: Loop2::circle(Vec2::new(15.0 + 3.0 + 2.0 + gap, 15.0), 2.0), holes: Vec::new() };
            assert_eq!(holds_clear(&region, &near_hole), ref_holds_clear(&region, &near_hole), "hole gap={gap:e}");
        }

        // Crossing, enclosing an existing hole, outside, inside an existing hole, and non-arc cubic loop.
        let crossing = Region2 { outer: Loop2::circle(Vec2::new(15.0, 15.0), 3.0), holes: Vec::new() };
        let enclosing = Region2 { outer: Loop2::circle(Vec2::new(15.0, 15.0), 4.5), holes: Vec::new() };
        let outside = Region2 { outer: Loop2::circle(Vec2::new(250.0, 100.0), 2.0), holes: Vec::new() };
        let inside_hole = Region2 { outer: Loop2::circle(Vec2::new(15.0, 15.0), 1.0), holes: Vec::new() };
        let cubic = Region2 {
            outer: Loop2 {
                segs: vec![
                    Seg2::Cubic { p0: Vec2::new(20.0, 20.0), p1: Vec2::new(21.0, 24.0), p2: Vec2::new(24.0, 24.0), p3: Vec2::new(25.0, 20.0) },
                    Seg2::Line { a: Vec2::new(25.0, 20.0), b: Vec2::new(20.0, 20.0) },
                ],
            },
            holes: Vec::new(),
        };
        for (name, cut) in [("crossing", crossing), ("enclosing", enclosing), ("outside", outside), ("inside_hole", inside_hole), ("cubic", cubic)] {
            assert_eq!(holds_clear(&region, &cut), ref_holds_clear(&region, &cut), "{name}");
        }
    }

    #[test]
    fn holes_apart_and_put_back_match_reference_for_many_holes() {
        let mut holes = Vec::with_capacity(155);
        for row in 0..10 {
            for col in 0..15 {
                let c = Vec2::new(15.0 + col as f64 * 15.0, 15.0 + row as f64 * 20.0);
                holes.push(Loop2::circle(c, 3.0).reversed());
            }
        }
        // Pair of touching holes (distance 0), hole grazing outer edge within CLEAR, and a clear hole
        // sharing the same x-column as others.
        holes.push(Loop2::circle(Vec2::new(240.0, 50.0), 3.0).reversed());
        holes.push(Loop2::circle(Vec2::new(240.0, 56.0), 3.0).reversed());
        holes.push(Loop2::circle(Vec2::new(3.0 + 0.5 * CLEAR, 100.0), 3.0).reversed());
        holes.push(Loop2::circle(Vec2::new(240.0, 150.0), 3.0).reversed());

        let region = Region2 { outer: rect_loop(0.0, 0.0, 260.0, 220.0), holes };
        let got = holes_apart(&region);
        let expected = ref_holes_apart(&region);
        assert_eq!(got, expected);
        assert!(got[..150].iter().all(|&ok| ok));
        assert!(!got[150] && !got[151], "touching holes must not be apart");
        assert!(!got[152], "hole within CLEAR of outer edge must not be apart");
        assert!(got[153], "disjoint hole in same x-column must remain apart");

        // Carry 120 holes back into two disjoint regions (one already containing a pre-existing hole).
        let base = vec![
            Region2 { outer: rect_loop(0.0, 0.0, 120.0, 120.0), holes: vec![Loop2::circle(Vec2::new(10.0, 10.0), 2.0).reversed()] },
            Region2 { outer: rect_loop(200.0, 0.0, 320.0, 120.0), holes: Vec::new() },
        ];
        let mut carried = Vec::with_capacity(120);
        for row in 0..6 {
            for col in 0..10 {
                carried.push(Loop2::circle(Vec2::new(20.0 + col as f64 * 10.0, 20.0 + row as f64 * 16.0), 2.0).reversed());
                carried.push(Loop2::circle(Vec2::new(210.0 + col as f64 * 10.0, 20.0 + row as f64 * 16.0), 2.0).reversed());
            }
        }
        let carried_with_origins = |holes: Vec<Loop2>| {
            holes
                .into_iter()
                .map(|h| {
                    let tags = vec![None; h.segs.len()];
                    (h, tags)
                })
                .collect()
        };
        let placed = put_back(Regions::unknown(base.clone()), carried_with_origins(carried.clone()), &mut FlatWork::new()).unwrap().geometry;
        let expected_placed = ref_put_back(base.clone(), carried).unwrap();
        assert_eq!(placed, expected_placed);
        assert_eq!(placed[0].holes.len(), 61);
        assert_eq!(placed[1].holes.len(), 60);

        // A hole inside an existing hole or outside all regions must fail identically to reference.
        assert!(
            put_back(
                Regions::unknown(base.clone()),
                carried_with_origins(vec![Loop2::circle(Vec2::new(10.0, 10.0), 0.5).reversed()]),
                &mut FlatWork::new()
            )
            .is_err()
        );
        assert!(
            put_back(Regions::unknown(base), carried_with_origins(vec![Loop2::circle(Vec2::new(160.0, 60.0), 2.0).reversed()]), &mut FlatWork::new())
                .is_err()
        );
    }

    #[test]
    fn trimming_a_previously_merged_boundary_does_not_copy_all_ancestors() {
        let region = |x0, x1| Region2 { outer: rect_loop(x0, 0.0, x1, 10.0), holes: Vec::new() };
        let origins = |base: usize| RegionOrigins {
            outer: (0..4).map(|k| Some(vec![ConvertedEdge { edge: base + k, faces: [base + k, base + k + 1] }])).collect(),
            holes: Vec::new(),
        };
        let first = origins(0);
        let second = origins(4);
        let whole = planar_boolean(
            &[region(0.0, 10.0)],
            &[region(10.0, 20.0)],
            BoolOp::Union,
            std::slice::from_ref(&first),
            std::slice::from_ref(&second),
            &mut FlatWork::new(),
        )
        .unwrap();
        assert_eq!(whole.geometry.len(), 1);
        assert_eq!(whole.origins.len(), 1);
        let bottom = Seg2::Line { a: Vec2::new(0.0, 0.0), b: Vec2::new(20.0, 0.0) };
        let at = whole.geometry[0].outer.segs.iter().position(|s| *s == bottom).expect("the real union must merge the two bottom edges");
        let parents = Some(vec![first.outer[0].as_ref().unwrap()[0], second.outer[0].as_ref().unwrap()[0]]);
        assert_eq!(whole.origins[0].outer[at], parents, "the setup must produce actual mixed parents before the second boolean");

        let unchanged = planar_boolean(&whole.geometry, &[], BoolOp::Union, &whole.origins, &[], &mut FlatWork::new()).unwrap();
        assert_eq!(unchanged.geometry, whole.geometry);
        assert_eq!(unchanged.origins, whole.origins, "an unchanged whole mixed boundary retains both known parents");

        // The tool stays outside y=0 and y=10, so it contributes neither bottom nor top.
        let tool = Region2 { outer: rect_loop(-1.0, -1.0, 5.0, 11.0), holes: Vec::new() };
        let shortened = Seg2::Line { a: Vec2::new(0.0, 0.0), b: Vec2::new(5.0, 0.0) };
        let singleton = planar_boolean(
            &[region(0.0, 20.0)],
            std::slice::from_ref(&tool),
            BoolOp::Intersect,
            std::slice::from_ref(&first),
            &[],
            &mut FlatWork::new(),
        )
        .unwrap();
        assert_eq!(singleton.geometry.len(), 1);
        let single_at = singleton.geometry[0].outer.segs.iter().position(|s| *s == shortened).expect("the singleton control retains x=0..5");
        assert_eq!(singleton.origins[0].outer[single_at], first.outer[0], "trimming one known parent preserves that parent");
        assert_eq!(singleton.geometry[0].area(), 50.0);

        let trimmed = planar_boolean(&whole.geometry, &[tool], BoolOp::Intersect, &whole.origins, &[], &mut FlatWork::new()).unwrap();
        assert_eq!(trimmed.geometry, singleton.geometry, "ancestral metadata does not change the boolean geometry");
        let trimmed_at = trimmed.geometry[0].outer.segs.iter().position(|s| *s == shortened).expect("the mixed control retains the same x=0..5 edge");
        // Only the left rectangle contributes to x=0..5. Without an input-extent certificate,
        // the old full mixed set cannot be certified for this strict subpiece.
        assert_eq!(trimmed.origins[0].outer[trimmed_at], None);
    }
    #[test]
    fn carried_hole_sources_keep_their_actual_geometry_index_after_unknown_history() {
        let region = Region2 {
            outer: rect_loop(0.0, 0.0, 20.0, 10.0),
            holes: vec![rect_loop(1.0, 2.0, 3.0, 4.0).reversed(), rect_loop(5.0, 2.0, 7.0, 4.0).reversed()],
        };
        let carried = rect_loop(11.0, 2.0, 13.0, 4.0).reversed();
        let tag = |edge| Some(vec![ConvertedEdge { edge, faces: [0, edge + 1] }]);
        let first = (4..8).map(tag).collect::<Vec<_>>();
        let second = (8..12).map(tag).collect::<Vec<_>>();
        let added = (12..16).map(tag).collect::<Vec<_>>();
        for holes in [Vec::new(), vec![first.clone()], vec![first.clone(), second.clone()]] {
            let before = holes.clone();
            let out = Regions { geometry: vec![region.clone()], origins: vec![RegionOrigins { outer: Vec::new(), holes }] };
            let result = put_back(out, vec![(carried.clone(), added.clone())], &mut FlatWork::new()).unwrap();
            assert_eq!(result.geometry[0].holes, vec![region.holes[0].clone(), region.holes[1].clone(), carried.clone()]);
            assert_eq!(result.origins[0].holes.len(), 3);
            assert_eq!(result.origins[0].holes[0], before.first().cloned().unwrap_or_default());
            assert_eq!(result.origins[0].holes[1], before.get(1).cloned().unwrap_or_default());
            assert_eq!(result.origins[0].holes[2], added);
        }
        let out = Regions { geometry: vec![region], origins: vec![RegionOrigins::default()] };
        let mut spent = FlatWork::new();
        assert!(!spent.charge_history(None));
        let result = put_back(out, vec![(carried, added)], &mut spent).unwrap();
        assert_eq!(result.geometry[0].holes.len(), 3);
        assert_eq!(result.origins[0], RegionOrigins::default(), "a declined padding charge cannot misassign a known hole");
    }
}
