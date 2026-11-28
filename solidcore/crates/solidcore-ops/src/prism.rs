//! Planar prisms: booleans of planar regions, the extrusion of planar material, whether a body
//! is a uniform prism (its section) or the same prism as another, and edges read in a plane.
//!
//! Each is a recipe of kernel calls (`solidcore::prism`): the kernel checks the profiles, splits
//! their loops, makes their curves, reads sheets back as regions, extrudes a region and proves a
//! prism; ops sequences those with the kernel's planar faces and 2D boolean, the record's
//! planar profiles and planes converted by `solidcore-convert` ([`convert::region2`],
//! [`convert::plane`] and back). It computes no geometric quantity.
//!
//! The booleans keep free-form curves exact: every boundary of a result is a piece of an
//! operand's boundary, cut where it crosses the other, never refitted.
//!
//! [`fill_channel`] is the kernel's channel fill (`solidcore::prism::fill`) in the record's
//! types, its typed refusal with the size that builds returned as a value.

// The range tests refuse NaN by negation (`!(x > y)`), kept as written.
#![allow(clippy::neg_cmp_op_on_partial_ord)]

use solidcore::boolean::api_brep::{boolean_2d, boolean_2d_with_edge_history, Boolean2dOutcome};
use solidcore::boolean::edge_history::{BoundaryHistory, EdgeSources};
use solidcore::error::Reason;
use solidcore::eval::Frame;
use solidcore::kernel::{Kernel, Tolerances};
use solidcore::primitive::api_trim_ops::{create_planar_faces, create_planar_faces_with_edges};
use solidcore::primitive::primitive_creation::Boolean2d;
use solidcore::prism::{extrude, fill, measure, profile as kp, read, sheet};
use solidcore::topology::EdgeId;
use solidcore::Brep;
use solidcore_convert as convert;
use solidcore_record::{
    BoundaryLoop, InputBoundary, Loop2, Plane, Region2, RegionWithHistory, Seg2, Vec2, Vec3,
};

pub use solidcore::error::fill::{FillFailure, FillSide};
pub use solidcore::prism::fill::{Contour, FillNote, FillOptions};
pub use solidcore::prism::measure::Interval;
pub use solidcore::prism::placement::LoopPlacement;
pub use solidcore::prism::profile::ProfileRefusal;

use crate::body::Contract;
use crate::body::{finish, Body, Expect};
use crate::boolean::{union_all_with, BooleanOp};
use crate::order::{self, Order};
use crate::session::{read_body, run, run_on, Answer};
use crate::{Error, Result};

/// Whether a generated cubic matches a trimmed interval of a source cubic, in either direction.
/// Other segment kinds give false.
#[must_use]
pub fn cubic_contains(source: &Seg2, piece: &Seg2) -> bool {
    convert::seg2(source).cubic_contains(&convert::seg2(piece))
}

/// The tolerance (mm) the planar booleans take: at least this.
pub const MIN_TOLERANCE: f64 = 1e-9;
/// ... and at most this.
pub const MAX_TOLERANCE: f64 = 1.0;
/// The tolerance the planar extrusion closes and splits its loops with (mm).
pub const EXTRUDE_TOLERANCE: f64 = 1e-7;

/// A call's own refusal, else its value, as a kernel call answers inside [`run`].
struct Out<T>(std::result::Result<T, Error>);

impl<T> Answer for Out<T> {
    fn refusal(&self) -> Option<Error> {
        self.0.as_ref().err().cloned()
    }
}

/// The frame the planar booleans work in: the regions' own coordinates.
fn regions_frame() -> Frame {
    convert::plane(&Plane::XY)
}

fn op2(op: BooleanOp) -> Boolean2d {
    match op {
        BooleanOp::Union => Boolean2d::Union,
        BooleanOp::Subtract => Boolean2d::Difference,
        BooleanOp::Intersect => Boolean2d::Intersection,
    }
}

/// A temporary sheet and, when requested, complete original-boundary tokens
/// for its actual kernel edges.
struct WorkingSheet {
    body: Brep,
    sources: Option<EdgeSources>,
}

impl WorkingSheet {
    fn plain(body: Brep) -> Self {
        Self {
            body,
            sources: None,
        }
    }
}

/// The one 2D-boolean recipe for both tracked and geometry-only sheets.
fn combine_working(
    k: &mut Kernel,
    a: WorkingSheet,
    b: WorkingSheet,
    op: BooleanOp,
    history: &mut BoundaryHistory,
) -> solidcore::Result<Option<WorkingSheet>> {
    let (outcome, sources) = match (a.sources, b.sources) {
        (Some(first), Some(second)) => {
            let (outcome, sources) =
                boolean_2d_with_edge_history(k, a.body, b.body, op2(op), &first, &second, history)?;
            (outcome, Some(sources))
        }
        _ => (boolean_2d(k, a.body, b.body, op2(op))?, None),
    };
    Ok(match outcome {
        Boolean2dOutcome::Result(body) if !body.face_list().is_empty() => {
            Some(WorkingSheet { body, sources })
        }
        Boolean2dOutcome::Result(_) | Boolean2dOutcome::Unassigned { .. } => None,
    })
}

/// Sheets united in the original balanced rounds, retaining the same refusal.
fn united_working(
    k: &mut Kernel,
    mut sheets: Vec<WorkingSheet>,
    history: &mut BoundaryHistory,
) -> solidcore::Result<std::result::Result<Option<WorkingSheet>, Error>> {
    while sheets.len() > 1 {
        let mut next = Vec::new();
        let mut iter = sheets.into_iter();
        while let Some(a) = iter.next() {
            next.push(match iter.next() {
                Some(b) => match combine_working(k, a, b, BooleanOp::Union, history)? {
                    Some(u) => u,
                    None => return Ok(Err(Error::Unsupported("a union of planar faces is empty"))),
                },
                None => a,
            });
        }
        sheets = next;
    }
    Ok(Ok(sheets.pop()))
}

/// Every curve is bound to its actual edge by p8's construction map, not edge order.
fn faces_working(
    k: &mut Kernel,
    loops: &[sheet::LoopWithData<Option<usize>>],
    tracked: bool,
    history: &mut BoundaryHistory,
) -> solidcore::Result<Vec<WorkingSheet>> {
    let f = regions_frame();
    loops
        .iter()
        .map(|lp| {
            let curves = lp
                .boundary
                .segs
                .iter()
                .map(|s| kp::curve_on_frame(k, &f, 0.0, s))
                .collect::<solidcore::Result<Vec<_>>>()?;
            if !tracked {
                return create_planar_faces(k, &curves).map(WorkingSheet::plain);
            }
            let (body, edges) = create_planar_faces_with_edges(k, &curves)?;
            let mut sources = EdgeSources::new();
            for (edge, curve) in edges {
                let value = lp
                    .data
                    .get(curve)
                    .copied()
                    .flatten()
                    .and_then(|id| history.original(id));
                // The constructor maps production slots. A repeated edge from distinct
                // slots has every source, while an unknown slot remains unknown.
                match sources.get(&edge) {
                    Some(Some(old)) => {
                        let merged = history.united(Some(old.clone()), value);
                        sources.insert(edge, merged);
                    }
                    Some(None) => {}
                    None => {
                        sources.insert(edge, value);
                    }
                }
            }
            for edge in solidcore::topology::brep::p1::edges(&body, None)? {
                sources.entry(edge).or_insert(None);
            }
            Ok(WorkingSheet {
                body,
                sources: Some(sources),
            })
        })
        .collect()
}

/// Each region's material united less its holes, then the regions united. The
/// normalization and boolean order are shared with the geometry-only recipe.
fn sheet_of_working(
    k: &mut Kernel,
    regions: &[kp::Region2],
    tol: f64,
    origins: Option<&[sheet::RegionWithData<usize>]>,
    history: &mut BoundaryHistory,
) -> solidcore::Result<std::result::Result<WorkingSheet, Error>> {
    let mut sheets = Vec::new();
    for (index, region) in regions.iter().enumerate() {
        let source = origins.and_then(|all| all.get(index));
        let outer: Vec<Option<usize>> = match source {
            Some(source) => source.outer.iter().copied().map(Some).collect(),
            None => region.outer.segs.iter().map(|_| None).collect(),
        };
        let holes: Vec<Vec<Option<usize>>> = match source {
            Some(source) => source
                .holes
                .iter()
                .map(|h| h.iter().copied().map(Some).collect())
                .collect(),
            None => region
                .holes
                .iter()
                .map(|h| h.segs.iter().map(|_| None).collect())
                .collect(),
        };
        let loops = sheet::region_loops_with_data(region, &outer, &holes, tol)?;
        let material = faces_working(k, &loops.material, origins.is_some(), history)?;
        let Some(material) = (match united_working(k, material, history)? {
            Ok(m) => m,
            Err(e) => return Ok(Err(e)),
        }) else {
            continue;
        };
        let holes = faces_working(k, &loops.holes, origins.is_some(), history)?;
        sheets.push(match united_working(k, holes, history)? {
            Ok(Some(holes)) => {
                match combine_working(k, material, holes, BooleanOp::Subtract, history)? {
                    Some(s) => s,
                    None => return Ok(Err(Error::Unsupported("the holes take all the material"))),
                }
            }
            Ok(None) => material,
            Err(e) => return Ok(Err(e)),
        });
    }
    Ok(match united_working(k, sheets, history)? {
        Ok(Some(s)) => Ok(s),
        Ok(None) => Err(Error::Empty),
        Err(e) => Err(e),
    })
}

fn sheet_of(
    k: &mut Kernel,
    regions: &[kp::Region2],
    tol: f64,
) -> solidcore::Result<std::result::Result<Brep, Error>> {
    Ok(sheet_of_working(k, regions, tol, None, &mut BoundaryHistory::untracked())?.map(|s| s.body))
}

/// The profile check's refusal as the call's, at the zone tolerance of the session's kernel,
/// which builds the profile: the kernel's default, since ops offers no way to change its
/// tolerances ([`crate::session`]), so the check accepts exactly the arcs the build makes.
fn checked<'a>(
    regions: impl Iterator<Item = &'a kp::Region2>,
    tol: f64,
) -> std::result::Result<(), Error> {
    kp::check_regions(regions, tol, Tolerances::default().zone_tol_3d).map_err(Error::Profile)
}

/// The boolean of planar regions bounded by lines, circular arcs and free-form segments, exact:
/// every boundary of the result is a piece of a boundary of `a` or `b`, cut where it crosses the
/// other. Disjoint results and retained islands are separate regions. `tolerance` (mm,
/// [`MIN_TOLERANCE`] to [`MAX_TOLERANCE`]) is how far apart a loop's segments may end and still
/// close it.
///
/// # Errors
/// [`Error::Argument`] for a tolerance out of range; [`Error::Profile`] for a profile the kernel
/// refuses; [`Error::Unsupported`] for a sheet the kernel cannot read back exactly; the kernel's
/// error.
pub fn planar_regions_boolean(
    a: &[Region2],
    b: &[Region2],
    op: BooleanOp,
    tolerance: f64,
) -> Result<Vec<Region2>> {
    planar_regions_boolean_impl(a, b, op, tolerance, false)
        .map(|regions| regions.into_iter().map(|r| r.region).collect())
}

/// The same planar boolean and complete source-boundary sets, aligned with every
/// output segment after canonical ordering. Indices name the exact input arrays.
/// None means unknown; no partial set is reported as complete.
///
/// # Errors
/// The same profile, tolerance and kernel errors as the geometry-only operation.
pub fn planar_regions_boolean_with_history(
    a: &[Region2],
    b: &[Region2],
    op: BooleanOp,
    tolerance: f64,
) -> Result<Vec<RegionWithHistory>> {
    planar_regions_boolean_impl(a, b, op, tolerance, true)
}

fn planar_regions_boolean_impl(
    a: &[Region2],
    b: &[Region2],
    op: BooleanOp,
    tolerance: f64,
    track_history: bool,
) -> Result<Vec<RegionWithHistory>> {
    if !(tolerance.is_finite() && (MIN_TOLERANCE..=MAX_TOLERANCE).contains(&tolerance)) {
        return Err(Error::Argument);
    }
    let (ka, kb): (Vec<_>, Vec<_>) = (
        a.iter().map(convert::region2).collect(),
        b.iter().map(convert::region2).collect(),
    );
    checked(ka.iter().chain(&kb), tolerance)?;
    // Count actual segments without allocating metadata. The profile check also
    // bounds them by MAX_SEGMENTS. If tracing cannot fit its existing read-work
    // allowance, the same geometry operation continues with unknown history.
    let track_history = track_history
        && ka
            .iter()
            .chain(&kb)
            .flat_map(kp::Region2::loops)
            .try_fold(sheet::READ_WORK, |remaining, lp| {
                remaining.checked_sub(lp.segs.len())
            })
            .is_some();
    let identity = |regions: &[Region2], operand| {
        regions
            .iter()
            .enumerate()
            .map(|(region, r)| {
                let one = |loop_kind, count| {
                    (0..count)
                        .map(|segment| {
                            track_history.then(|| {
                                vec![InputBoundary {
                                    operand,
                                    region,
                                    loop_kind,
                                    segment,
                                    whole: true,
                                }]
                            })
                        })
                        .collect()
                };
                RegionWithHistory {
                    region: r.clone(),
                    outer: one(BoundaryLoop::Outer, r.outer.segs.len()),
                    holes: r
                        .holes
                        .iter()
                        .enumerate()
                        .map(|(h, lp)| one(BoundaryLoop::Hole(h), lp.segs.len()))
                        .collect(),
                }
            })
            .collect()
    };
    match (a.is_empty(), b.is_empty(), op) {
        (true, _, BooleanOp::Union) => return Ok(identity(b, 1)),
        (true, _, _) | (false, true, BooleanOp::Intersect) => return Ok(Vec::new()),
        (false, true, _) => return Ok(identity(a, 0)),
        _ => {}
    }
    let mut tokens: Vec<InputBoundary> = Vec::new();
    let mut keyed = |regions: &[kp::Region2], operand| {
        regions
            .iter()
            .enumerate()
            .map(|(region, r)| {
                let mut one = |loop_kind, count| {
                    (0..count)
                        .map(|segment| {
                            let id = tokens.len();
                            tokens.push(InputBoundary {
                                operand,
                                region,
                                loop_kind,
                                segment,
                                whole: true,
                            });
                            id
                        })
                        .collect()
                };
                sheet::RegionWithData {
                    region: r.clone(),
                    outer: one(BoundaryLoop::Outer, r.outer.segs.len()),
                    holes: r
                        .holes
                        .iter()
                        .enumerate()
                        .map(|(h, lp)| one(BoundaryLoop::Hole(h), lp.segs.len()))
                        .collect(),
                }
            })
            .collect::<Vec<_>>()
    };
    let oa = track_history.then(|| keyed(&ka, 0));
    let ob = track_history.then(|| keyed(&kb, 1));
    let mut history = if track_history {
        BoundaryHistory::default()
    } else {
        BoundaryHistory::untracked()
    };
    let out = run("planar_regions_boolean", |k| {
        let sa = match sheet_of_working(k, &ka, tolerance, oa.as_deref(), &mut history)? {
            Ok(s) => s,
            Err(e) => return Ok(Out(Err(e))),
        };
        let sb = match sheet_of_working(k, &kb, tolerance, ob.as_deref(), &mut history)? {
            Ok(s) => s,
            Err(e) => return Ok(Out(Err(e))),
        };
        let Some(r) = combine_working(k, sa, sb, op, &mut history)? else {
            return Ok(Out(Ok((Vec::new(), None))));
        };
        // Carry only actual edge IDs through loop splits and ordering. Expand
        // source vectors once afterwards under the existing sheet read budget.
        let read = sheet::sheet_regions_with_data(
            k,
            &r.body,
            &regions_frame(),
            tolerance,
            |edge, whole| (edge, whole),
        )?;
        Ok(Out(Ok((read.regions, r.sources))))
    })?;
    let (regions, sources) = out.0?;
    let mut remaining = sheet::READ_WORK;
    let mut origins = |(edge, whole): (EdgeId, bool)| -> Option<Vec<InputBoundary>> {
        let certificate = sources.as_ref()?.get(&edge)?.as_ref()?;
        let values = history.sources(certificate)?;
        if !whole && values.len() != 1 {
            return None;
        }
        remaining = remaining.checked_sub(values.len())?;
        let mut values = values
            .iter()
            .map(|source| {
                tokens.get(source.input).copied().map(|mut origin| {
                    origin.whole &= source.whole && whole;
                    origin
                })
            })
            .collect::<Option<Vec<_>>>()?;
        if values.is_empty() {
            return None;
        }
        values.sort_unstable();
        values.dedup();
        Some(values)
    };
    Ok(regions
        .into_iter()
        .map(|r| RegionWithHistory {
            region: convert::record_region2(&r.region),
            outer: r.outer.into_iter().map(&mut origins).collect(),
            holes: r
                .holes
                .into_iter()
                .map(|h| h.into_iter().map(&mut origins).collect())
                .collect(),
        })
        .collect())
}

/// Normalized planar material extruded as one body, keeping every disjoint region and its
/// holes: the regions united in the plane first (overlapping ones merge), pieces of the union
/// that touch refused, each region extruded from `start` to `end` above `plane`
/// ([`extrude::extrude_region`]) and the solids joined ([`union_all`]).
///
/// # Errors
/// [`Error::Argument`] for no regions; [`Error::Profile`] for a profile the kernel refuses;
/// [`Error::Unsupported`] for pieces that touch, or a sheet not read back exactly;
/// [`Error::Empty`] when no material is left; the kernel's error.
pub fn extrude_planar_regions(
    plane: &Plane,
    regions: &[Region2],
    start: f64,
    end: f64,
) -> Result<Body> {
    if regions.is_empty() {
        return Err(Error::Argument);
    }
    let kr: Vec<_> = regions.iter().map(convert::region2).collect();
    checked(kr.iter(), EXTRUDE_TOLERANCE)?;
    let target = convert::plane(plane);
    let pieces = run("extrude_planar_regions", |k| {
        let union = match sheet_of(k, &kr, EXTRUDE_TOLERANCE)? {
            Ok(s) => s,
            Err(e) => return Ok(Out(Err(e))),
        };
        let read = sheet::sheet_regions(k, &union, &regions_frame(), EXTRUDE_TOLERANCE)?;
        if read.touching {
            return Ok(Out(Err(Error::Unsupported(
                "planar material pieces touch at a boundary",
            ))));
        }
        if read.regions.is_empty() {
            return Ok(Out(Err(Error::Empty)));
        }
        let mut solids = Vec::new();
        for r in &read.regions {
            let b = extrude::extrude_region(k, &target, r, start, end)?;
            match finish(k, b, Expect::SOLID)? {
                Ok(body) => solids.push(body),
                Err(e) => return Ok(Out(Err(e))),
            }
        }
        Ok(Out(Ok(solids)))
    })?;
    union_all_with(
        pieces.0?.into_iter().map(Body::new).collect(),
        Contract::Proved,
    )
}

/// A uniform prism along a plane's normal, as [`prism_section`] proves it.
#[derive(Clone, Debug, PartialEq)]
pub struct PrismSection {
    /// The level of its bottom cap above the plane.
    pub lo: f64,
    /// The level of its top cap (`lo < hi`).
    pub hi: f64,
    /// Its section, exact: lines and arcs, the outer loop counter-clockwise, the holes clockwise.
    pub region: Region2,
    /// How far the levels and section may be off the body's caps, including frame conversion.
    pub tolerance: f64,
    /// An enclosure of its volume.
    pub volume: Interval,
}

impl Answer for PrismSection {}

/// The uniform prism `body` is along `plane`'s normal, when the kernel proves it one on its
/// current geometry ([`measure::section`]): `None` when that is not proved.
///
/// # Errors
/// Those of the session.
pub fn prism_section(body: &Body, plane: &Plane) -> Result<Option<PrismSection>> {
    let f = convert::plane(plane);
    let s = read_body("prism_section", body, |k, b| Ok(measure::section(k, b, &f)))?;
    Ok(s.map(|s| PrismSection {
        lo: s.lo,
        hi: s.hi,
        region: convert::record_region2(&s.region),
        tolerance: s.tolerance,
        volume: s.volume,
    }))
}

/// Whether `a` and `b` are proved the same uniform prism ([`measure::equivalent`]): `None` when
/// that is not proved either way (never a copy granted).
///
/// # Errors
/// Those of the session.
pub fn prism_equivalent(a: &Body, b: &Body) -> Result<Option<bool>> {
    if std::ptr::eq(a, b) {
        return read_body("prism_equivalent", a, |k, g| {
            Ok(measure::equivalent(k, g, g))
        });
    }
    run_on("prism_equivalent", &[("a", a), ("b", b)], |k| {
        let (ga, gb) = Body::lock_pair(a, b);
        Ok(measure::equivalent(k, &ga, &gb))
    })
}

pub use solidcore_record::projection::{PlanarPiece, PlanarSpline};

/// Edges read in a plane, with the tolerance they were read to.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanarEdges {
    /// How far a piece read as a line or arc may lie from its edge (mm).
    pub tolerance: Option<f64>,
    /// Each edge's pieces, `None` for one not read exactly in a plane parallel to the plane.
    pub pieces: Vec<Option<Vec<PlanarPiece>>>,
}

impl Answer for PlanarEdges {}

/// Record edges `edges` of `body` in `plane`'s coordinates, exactly ([`read::planar_edges`]):
/// lines, circular arcs, single free-form segments, and a free-form curve of several spans as the
/// one B-spline it is, for a sketch to project them.
///
/// # Errors
/// [`Error::Argument`] for an index past the edges; those of the session.
pub fn planar_edges(
    body: &Body,
    order: &Order,
    plane: &Plane,
    edges: &[usize],
) -> Result<PlanarEdges> {
    let ids = edges
        .iter()
        .map(|i| order::edge(order, *i))
        .collect::<Result<Vec<_>>>()?;
    let f = convert::plane(plane);
    read_body("planar_edges", body, |k, b| {
        let pieces = read::planar_edges(k, b, &f, &ids)
            .into_iter()
            .map(|e| {
                e.map(|ps| {
                    ps.into_iter()
                        .map(|p| match p {
                            read::PlanarPiece::Seg(s) => PlanarPiece::Seg(convert::record_seg2(&s)),
                            read::PlanarPiece::Spline(s) => PlanarPiece::Spline(PlanarSpline {
                                degree: s.degree,
                                knots: s.knots,
                                points: s.points.into_iter().map(convert::record_vec2).collect(),
                                weights: s.weights,
                            }),
                        })
                        .collect()
                })
            })
            .collect();
        Ok(PlanarEdges {
            tolerance: read::planar_edges_tolerance(b),
            pieces,
        })
    })
}

/// The signed area of `region` (its loops' signed areas summed), with a proved bound on its
/// rounding ([`kp::Region2::signed_area`]).
///
/// # Errors
/// [`Error::Profile`] for a malformed segment.
pub fn region_area(region: &Region2) -> Result<kp::Area> {
    convert::region2(region)
        .signed_area()
        .map_err(|_| Error::Profile(ProfileRefusal::FreeForm))
}

/// The section loop a closed chain of nodes samples ([`solidcore::prism::fit::fit_loop`]): a
/// full circle where the nodes are one within `tolerance`, else the polygon of their corners,
/// each straight run joined within three times `epsilon`. For a mesh's cap boundary, which the
/// caller certifies against the mesh afterwards.
///
/// # Errors
/// [`Error::Status`] (input refused) past the straight-run join's work bound.
pub fn fitted_loop(points: &[Vec2], tolerance: f64, epsilon: f64) -> Result<Loop2> {
    let points: Vec<_> = points.iter().map(|p| convert::vec2(*p)).collect();
    Ok(
        match solidcore::prism::fit::fit_loop(&points, tolerance, epsilon)
            .map_err(crate::error::status)?
        {
            solidcore::prism::fit::FittedLoop::Circle { center, radius } => {
                Loop2::circle(convert::record_vec2(center), radius)
            }
            solidcore::prism::fit::FittedLoop::Polygon(corners) => Loop2::polygon(
                &corners
                    .into_iter()
                    .map(convert::record_vec2)
                    .collect::<Vec<_>>(),
            ),
        },
    )
}

/// Whether `lp` holds `p` by the even–odd rule, decided exactly ([`kp::Loop2::holds`]): `None`
/// when `p` is on the loop or within rounding of it.
#[must_use]
pub fn loop_holds(lp: &Loop2, p: Vec2) -> Option<bool> {
    convert::loop2(lp).holds(convert::vec2(p))
}

/// How far `p` is from `seg`, exactly, for a line or a circular arc ([`kp::Seg2::distance_to`]):
/// the foot clamped to a line's ends; on an arc the radial distance where `p` is within its angles,
/// else the distance to the nearer end. `None` for a free-form segment or a point that is no number.
#[must_use]
pub fn segment_distance(seg: &Seg2, p: Vec2) -> Option<f64> {
    convert::seg2(seg).distance_to(convert::vec2(p))
}

/// Whether the circles `a` and `b` (centre and radius) are one hole: their centres and their radii
/// each less than `tol` apart ([`solidcore::develop::same_circle`]).
#[must_use]
pub fn same_circle(a: (Vec2, f64), b: (Vec2, f64), tol: f64) -> bool {
    solidcore::develop::same_circle((convert::vec2(a.0), a.1), (convert::vec2(b.0), b.1), tol)
}

/// Where a note `height` high stands to name the circle `at` (centre and radius), level with its
/// centre and clear of its rim on the right: its lower left corner
/// ([`solidcore::develop::note_beside`]).
#[must_use]
pub fn note_beside(at: (Vec2, f64), height: f64) -> Vec2 {
    convert::record_vec2(solidcore::develop::note_beside(
        (convert::vec2(at.0), at.1),
        height,
    ))
}

/// Counter-clockwise angle between two planar rays, in radians modulo one turn.
/// Equal rays give zero. Nonfinite and zero rays have no angle.
#[must_use]
pub fn counterclockwise_sweep(center: Vec2, start: Vec2, end: Vec2) -> Option<f64> {
    kp::counterclockwise_sweep(
        convert::vec2(center),
        convert::vec2(start),
        convert::vec2(end),
    )
}

/// The directed parameter interval of a recognized ellipse arc, in the input plane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EllipseArc {
    /// The centre in the input plane.
    pub center: Vec2,
    /// The directed major semiaxis.
    pub major: Vec2,
    /// The positive minor-to-major axis ratio.
    pub ratio: f64,
    /// The starting parameter in radians.
    pub start: f64,
    /// The ending parameter in radians, retaining direction.
    pub end: f64,
}

/// Recognize the supporting ellipse of the given rational quadratic, or refuse.
pub fn conic_ellipse(a: Vec2, apex: Vec2, b: Vec2, w: f64) -> Option<EllipseArc> {
    let arc = solidcore::prism::recognition::conic_ellipse(
        convert::vec2(a),
        convert::vec2(apex),
        convert::vec2(b),
        w,
    )?;
    Some(EllipseArc {
        center: convert::record_vec2(arc.center),
        major: convert::record_vec2(arc.major),
        ratio: arc.ratio,
        start: arc.start,
        end: arc.end,
    })
}

/// Whether a piece of a planar curve within the reading tolerance of its chord is read as the
/// arc it is proved on before that line ([`read::arc_before_line`]): `spans` its Bézier spans
/// (`[x w, y w, w]` in the plane), `zone` the model's zone tolerance
/// ([`crate::sweep::zone_tolerance`]).
#[must_use]
pub fn arc_before_line(spans: &[Vec<[f64; 3]>], zone: f64) -> bool {
    read::arc_before_line(spans, zone)
}

/// Recognize one directed turn of arcs with the same stored support.
pub fn loop_circle(lp: &Loop2) -> Option<(Vec2, f64)> {
    if lp.segs.len() > 200_000 {
        return None;
    }
    let (center, radius) = solidcore::prism::recognition::loop_circle(&convert::loop2(lp))?;
    Some((convert::record_vec2(center), radius))
}

/// Recognize one ellipse turn without changing any input control point.
pub fn loop_ellipse(lp: &Loop2) -> Option<(Vec2, Vec2, f64)> {
    if lp.segs.len() > 200_000 {
        return None;
    }
    let (center, major, ratio) = solidcore::prism::recognition::loop_ellipse(&convert::loop2(lp))?;
    Some((
        convert::record_vec2(center),
        convert::record_vec2(major),
        ratio,
    ))
}

/// Read the signed single winding of ordered points already admitted on one round wall.
pub fn certified_round_wall_turn(points: &[Vec2], center: Vec2) -> Option<f64> {
    if points.len() > 200_000 {
        return None;
    }
    solidcore::prism::recognition::certified_round_wall_turn(
        &points
            .iter()
            .copied()
            .map(convert::vec2)
            .collect::<Vec<_>>(),
        convert::vec2(center),
    )
}

/// Where `lp` lies against `other` (both in one plane's coordinates), loops within `tol` of each
/// other counting as meeting ([`kp::Loop2::placement`]): clear inside,
/// clear outside, along it everywhere, or meeting it in places only. `None` for loops of other
/// than lines and arcs, or a placement the kernel cannot decide.
#[must_use]
pub fn loop_placement(lp: &Loop2, other: &Loop2, tol: f64) -> Option<LoopPlacement> {
    convert::loop2(lp).placement(&convert::loop2(other), tol)
}

/// Whether the closed loop `lp` (in one plane's coordinates) crosses or touches itself anywhere
/// but where its neighbouring pieces join, pieces within `tol` of each other counting as meeting
/// ([`kp::loop_crosses`]): what a loop moved past a neck does
/// ([`crate::develop::offset_closed_contour`]). `None` for a loop with a segment other than a
/// line or an arc, or a `tol` that is not positive and finite.
#[must_use]
pub fn loop_crosses_itself(lp: &Loop2, tol: f64) -> Option<bool> {
    let lp = convert::loop2(lp);
    (tol.is_finite() && tol > 0.0 && lp.segs.iter().all(kp::Seg2::is_line_or_arc))
        .then(|| kp::loop_crosses(&lp.segs, &[], tol))
}

/// Positive and negative wing `(start, end)` intervals for a symmetric-offset extrude.
#[must_use]
pub fn symmetric_offset_intervals(gap: f64, half: f64) -> [(f64, f64); 2] {
    extrude::symmetric_offset_intervals(gap, half)
}

/// Probe points along both wings of a symmetric-offset extrude for auto-operation detection.
#[must_use]
pub fn symmetric_offset_probe_points(
    base: Vec3,
    n: Vec3,
    start: f64,
    d: f64,
    near_limit: f64,
) -> [Vec3; 4] {
    extrude::symmetric_offset_probe_points(
        [base.x, base.y, base.z],
        [n.x, n.y, n.z],
        start,
        d,
        near_limit,
    )
    .map(|[x, y, z]| Vec3::new(x, y, z))
}

/// Where the line along `sketch`'s normal through `point` meets `plane`, as a level along that
/// normal from `sketch`'s origin ([`extrude::level_on_plane`]): an extrude's end on a flat face
/// at a slant to it.
///
/// # Errors
///
/// [`Error::Refused`] for a plane along the normal or a level that is not finite.
pub fn level_on_plane(sketch: &Plane, point: Vec3, plane: &Plane) -> Result<f64> {
    extrude::level_on_plane(
        &convert::plane(sketch),
        convert::vec3(point),
        &convert::plane(plane),
    )
    .map_err(crate::error::status)
}

/// The least and the most level of `plane` above `sketch` over the outer loops of `regions`
/// ([`extrude::plane_rise`]): how far a flat face at a slant to an extrude rises across its
/// profiles.
///
/// # Errors
///
/// [`Error::Refused`] for a plane along the normal, no outer segment or a malformed segment;
/// [`Error::Status`] for a free-form piece whose search does not settle.
pub fn plane_rise(sketch: &Plane, plane: &Plane, regions: &[Region2]) -> Result<(f64, f64)> {
    let regions: Vec<_> = regions.iter().map(convert::region2).collect();
    extrude::plane_rise(&convert::plane(sketch), &convert::plane(plane), &regions)
        .map_err(crate::error::status)
}

/// The least and the most of `to`'s level above `sketch` less `from`'s over the outer loops of
/// `regions` ([`extrude::plane_gap`]): the gap an extrude From one flat face To another runs
/// across its profiles.
///
/// # Errors
///
/// As [`plane_rise`].
pub fn plane_gap(
    sketch: &Plane,
    from: &Plane,
    to: &Plane,
    regions: &[Region2],
) -> Result<(f64, f64)> {
    let regions: Vec<_> = regions.iter().map(convert::region2).collect();
    extrude::plane_gap(
        &convert::plane(sketch),
        &convert::plane(from),
        &convert::plane(to),
        &regions,
    )
    .map_err(crate::error::status)
}

/// `plane` moved by `by` along `sketch`'s normal, its axes kept ([`extrude::plane_raised`]): an
/// extrude's end on a slanted face moved along the extrusion.
///
/// # Errors
///
/// [`Error::Refused`] for a distance or an origin that is not finite.
pub fn plane_raised(sketch: &Plane, plane: &Plane, by: f64) -> Result<Plane> {
    extrude::plane_raised(&convert::plane(sketch), &convert::plane(plane), by)
        .map(|f| convert::record_plane(&f))
        .map_err(crate::error::status)
}

/// Which side of `from` and of `to` holds what lies past `from` and short of `to` along
/// `travel`, each `true` for the side the plane's normal points to
/// ([`extrude::slab_sides`]): the two cuts that keep an extrude's tools between its ends.
///
/// # Errors
///
/// [`Error::Refused`] for a plane along `travel`, or a `travel` that is zero or not finite.
pub fn slab_sides(travel: Vec3, from: &Plane, to: &Plane) -> Result<(bool, bool)> {
    extrude::slab_sides(
        convert::vec3(travel),
        &convert::plane(from),
        &convert::plane(to),
    )
    .map_err(crate::error::status)
}

/// A channel filled by [`fill_channel`] ([`fill::Filled`] in the record's types).
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelFill {
    /// The filled section: one counter-clockwise loop, no holes.
    pub region: Region2,
    /// How deep the channel reaches, measured as the depth is.
    pub channel_depth: f64,
    /// How deep it is filled.
    pub filled_depth: f64,
    /// Whether the channel is open on the far side.
    pub through: bool,
    /// What a caller may report.
    pub notes: Vec<FillNote>,
}

/// Why [`fill_channel`] refuses a fill, and the size that builds where there is one (the
/// kernel's [`FillFailure`] and its fit).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FillRefusal {
    /// Why.
    pub why: FillFailure,
    /// The nearest size that builds (a face's height or depth), where there is one.
    pub fit: Option<f64>,
}

/// [`fill_channel`]'s answer inside [`run`]: a fill or its refusal, neither an error.
struct Filling(std::result::Result<ChannelFill, FillRefusal>);

impl Answer for Filling {}

/// The channel between the walls `left` and `right` filled with `options`
/// ([`fill::fill`]): the depth measured along `depth_normal` from the deeper toe where it is
/// given, else from the chord. A refusal the kernel types ([`FillFailure`]) comes back as
/// `Ok(Err(..))` with its fit.
///
/// # Errors
/// [`Error::Refused`] for walls, a normal, contours or a depth the kernel refuses as input;
/// the kernel's other errors.
pub fn fill_channel(
    left: &[Seg2],
    right: &[Seg2],
    depth_normal: Option<Vec2>,
    options: &FillOptions,
) -> Result<std::result::Result<ChannelFill, FillRefusal>> {
    let channel = fill::Channel {
        left: left.iter().map(convert::seg2).collect(),
        right: right.iter().map(convert::seg2).collect(),
        depth_normal: depth_normal.map(convert::vec2),
    };
    let out = run("fill_channel", |k| match fill::fill(k, &channel, options) {
        Ok(f) => Ok(Filling(Ok(ChannelFill {
            region: convert::record_region2(&f.region),
            channel_depth: f.channel_depth,
            filled_depth: f.filled_depth,
            through: f.through,
            notes: f.notes,
        }))),
        Err(e) => match e.failure().map(|f| &f.reason) {
            Some(&Reason::Fill(why)) => Ok(Filling(Err(FillRefusal {
                why,
                fit: e.fit().map(|f| f.size),
            }))),
            _ => Err(e),
        },
    })?;
    Ok(out.0)
}
