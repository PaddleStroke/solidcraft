//! Contour edits past the end of a shared flange corner.

use super::plane::Planar;
use super::refusal::CornerTailRefusal;
use crate::math::Vec2;
use crate::prism::profile::{Loop2, Region2, Seg2};

/// One original outer segment consumed by a contour edit. `whole` is false when
/// the complete input is not certified retained. These are construction indices, not matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CornerTailParent {
    /// Index in the original input contour's outer loop.
    pub segment: usize,
    /// Whether this output certifies that the complete original parent remains.
    pub whole: bool,
}

/// Geometry and optional complete parent sets for its actual outer segments.
/// The current construction consumes at most three original contour segments when
/// removing a previous step; splitting that result adds no parents. New bridges
/// and any set that cannot fit those three slots are explicitly unknown.
#[derive(Clone, Debug)]
pub struct CornerTailWithHistory {
    /// The same edited geometry returned by the geometry-only entrypoint.
    pub region: Region2,
    /// Complete original-input parents per output segment; missing or None is unknown.
    pub outer: Vec<Option<[Option<CornerTailParent>; 3]>>,
}

fn identity(segment: usize) -> [Option<CornerTailParent>; 3] {
    [
        Some(CornerTailParent {
            segment,
            whole: true,
        }),
        None,
        None,
    ]
}

fn compose(
    input: &[Option<[Option<CornerTailParent>; 3]>],
    parents: Option<[Option<CornerTailParent>; 3]>,
) -> Option<[Option<CornerTailParent>; 3]> {
    let mut result: [Option<CornerTailParent>; 3] = [None; 3];
    for parent in parents?.into_iter().flatten() {
        for mut source in input
            .get(parent.segment)
            .copied()
            .flatten()?
            .into_iter()
            .flatten()
        {
            source.whole &= parent.whole;
            if let Some(existing) = result
                .iter_mut()
                .flatten()
                .find(|p| p.segment == source.segment)
            {
                existing.whole &= source.whole;
            } else {
                *result.iter_mut().find(|p| p.is_none())? = Some(source);
            }
        }
    }
    Some(result)
}

/// Sets a straight side back, preserving holes in `region`.
///
/// `from` is the side's base in the panel chart, `m` its unit direction,
/// and `rise` its original length. `h` is the distance from `from` where the
/// setback begins; `into` is the unit direction into the panel and `by` is
/// the setback distance.
///
/// Returns `None` if the straight side or its straight successor cannot be
/// found, or the step cannot meet that successor without reversing it.
pub fn set_back_past(
    region: &Region2,
    from: Vec2,
    m: Vec2,
    rise: f64,
    h: f64,
    into: Vec2,
    by: f64,
) -> Option<Region2> {
    set_back_past_impl(region, from, m, rise, h, into, by, false).map(|out| out.region)
}

/// The same contour edit with complete construction parents for its outer segments.
pub fn set_back_past_with_history(
    region: &Region2,
    from: Vec2,
    m: Vec2,
    rise: f64,
    h: f64,
    into: Vec2,
    by: f64,
) -> Option<CornerTailWithHistory> {
    set_back_past_impl(region, from, m, rise, h, into, by, true)
}

fn set_back_past_impl(
    region: &Region2,
    from: Vec2,
    m: Vec2,
    rise: f64,
    h: f64,
    into: Vec2,
    by: f64,
    trace: bool,
) -> Option<CornerTailWithHistory> {
    let segs = &region.outer.segs;
    let n = segs.len();
    let top = from + m * rise;
    let tol = rise.max(1.0) * 1e-9;
    let same = |p: Vec2, q: Vec2| p.dist(q) <= tol;
    let (k, side) = segs
        .iter()
        .enumerate()
        .find(|(_, g)| matches!(**g, Seg2::Line { a, b } if (same(a, from) && same(b, top)) || (same(a, top) && same(b, from))))?;
    let up = matches!(*side, Seg2::Line { a, .. } if same(a, from));
    let past_at = if up {
        k.checked_add(1).filter(|next| *next < n).unwrap_or(0)
    } else {
        previous(k, n).unwrap_or(0)
    };
    let Seg2::Line { a: pa, b: pb } = *segs.get(past_at)? else {
        return None;
    };
    let (p, q) = (from + m * h, from + m * h + into * by);
    let w = pb - pa;
    let cross = m.cross(w);
    if cross.abs() <= 1e-12 {
        return None;
    }
    let ahead = (pa - q).cross(w) / cross;
    let mut pts = if ahead > 0.0 {
        vec![from, p, q, q + m * ahead]
    } else {
        let across = into.cross(w);
        if across.abs() <= 1e-12 {
            return None;
        }
        vec![from, p, p + into * ((pa - p).cross(w) / across)]
    };
    let end = *pts.last()?;
    let kept = if up { pb - end } else { end - pa };
    if !(kept.dot(w) > 0.0) {
        return None;
    }
    if !up {
        pts.reverse();
    }
    let side: Vec<Seg2> = pts
        .windows(2)
        .filter_map(|pair| {
            let [a, b] = pair else { return None };
            Some(Seg2::Line { a: *a, b: *b })
        })
        .collect();
    let mut parents = if trace {
        (0..n)
            .map(|segment| Some(identity(segment)))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if let Some(source) = parents.get_mut(past_at) {
        *source = Some([
            Some(CornerTailParent {
                segment: past_at,
                whole: false,
            }),
            None,
            None,
        ]);
    }
    let mut side_parents = if trace {
        vec![None; side.len()]
    } else {
        Vec::new()
    };
    if let Some(first) = side_parents.first_mut() {
        *first = Some([
            Some(CornerTailParent {
                segment: k,
                whole: false,
            }),
            None,
            None,
        ]);
    }
    if pts.len() == 4 {
        if let Some(last) = side_parents.last_mut() {
            *last = Some([
                Some(CornerTailParent {
                    segment: k,
                    whole: false,
                }),
                None,
                None,
            ]);
        }
    }
    if !up {
        side_parents.reverse();
    }
    let mut out = segs.clone();
    *out.get_mut(past_at)? = if up {
        Seg2::Line { a: end, b: pb }
    } else {
        Seg2::Line { a: pa, b: end }
    };
    out.splice(k..=k, side);
    if trace {
        parents.splice(k..=k, side_parents);
    }
    Some(CornerTailWithHistory {
        region: Region2 {
            outer: Loop2 { segs: out },
            holes: region.holes.clone(),
        },
        outer: parents,
    })
}

fn previous(index: usize, count: usize) -> Option<usize> {
    index.checked_sub(1).or_else(|| count.checked_sub(1))
}

/// Restores a previous corner setback and applies the new setback to the same side.
///
/// `region` is the panel outline and `edge` gives the shared side's endpoints
/// in its chart. `anchor_origin` and unit `anchor_direction` define the bend
/// edge from its start, and `bend_length` is the bend's length along that edge.
/// `old_cut` and `new_cut` are the previous and desired setback distances.
///
/// # Errors
/// The contour cannot be edited unambiguously or with straight edges.
pub fn retarget_corner_tail(
    region: &Region2,
    edge: [Vec2; 2],
    anchor_origin: Vec2,
    anchor_direction: Vec2,
    bend_length: f64,
    old_cut: f64,
    new_cut: f64,
) -> Result<Region2, CornerTailRefusal> {
    retarget_corner_tail_impl(
        region,
        edge,
        anchor_origin,
        anchor_direction,
        bend_length,
        old_cut,
        new_cut,
        false,
    )
    .map(|out| out.region)
}

/// The same contour edit with complete construction parents for its outer segments.
pub fn retarget_corner_tail_with_history(
    region: &Region2,
    edge: [Vec2; 2],
    anchor_origin: Vec2,
    anchor_direction: Vec2,
    bend_length: f64,
    old_cut: f64,
    new_cut: f64,
) -> Result<CornerTailWithHistory, CornerTailRefusal> {
    retarget_corner_tail_impl(
        region,
        edge,
        anchor_origin,
        anchor_direction,
        bend_length,
        old_cut,
        new_cut,
        true,
    )
}

fn retarget_corner_tail_impl(
    region: &Region2,
    edge: [Vec2; 2],
    anchor_origin: Vec2,
    anchor_direction: Vec2,
    bend_length: f64,
    old_cut: f64,
    new_cut: f64,
    trace: bool,
) -> Result<CornerTailWithHistory, CornerTailRefusal> {
    let [start, end] = edge;
    let distance = |point: Vec2| anchor_direction.cross(point - anchor_origin).abs();
    let (from, end) = if distance(start) <= distance(end) {
        (start, end)
    } else {
        (end, start)
    };
    let direction = (end - from)
        .normalized()
        .ok_or(CornerTailRefusal::NoLength)?;
    let along = if (from - anchor_origin).dot(anchor_direction) < bend_length / 2.0 {
        anchor_direction
    } else {
        -anchor_direction
    };
    let into = (along - direction * along.dot(direction))
        .normalized()
        .ok_or(CornerTailRefusal::AlongBend)?;
    let rise_axis = if along.perp().dot(direction) > 0.0 {
        along.perp()
    } else {
        -along.perp()
    };
    replace_setback(region, from, end, into, rise_axis, old_cut, new_cut, trace)
}

fn replace_setback(
    region: &Region2,
    from: Vec2,
    end: Vec2,
    into: Vec2,
    rise_axis: Vec2,
    old_cut: f64,
    new_cut: f64,
    trace: bool,
) -> Result<CornerTailWithHistory, CornerTailRefusal> {
    let height = from.dist(end);
    let direction = (end - from)
        .normalized()
        .ok_or(CornerTailRefusal::NoLength)?;
    let tolerance = height.max(1.0) * 1e-9;
    let same = |first: Vec2, second: Vec2| first.dist(second) <= tolerance;
    let old_end = end + into * old_cut;
    let mut restored = region.clone();
    let mut origins = if trace {
        (0..region.outer.segs.len())
            .map(|segment| Some(identity(segment)))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if old_cut.abs() > 1e-12 {
        let mut steps = region
            .outer
            .segs
            .iter()
            .enumerate()
            .filter_map(|(index, segment)| match *segment {
                Seg2::Line { a, b } if same(a, end) && same(b, old_end) => Some((index, true)),
                Seg2::Line { a, b } if same(a, old_end) && same(b, end) => Some((index, false)),
                _ => None,
            });
        if let Some((step, forward)) = steps.next() {
            if steps.next().is_some() || region.outer.segs.len() < 4 {
                return Err(CornerTailRefusal::AmbiguousTrim);
            }
            let count = region.outer.segs.len();
            let start = previous(step, count).ok_or(CornerTailRefusal::MissingContour)?;
            let mut segments: Vec<_> = region
                .outer
                .segs
                .iter()
                .cycle()
                .skip(start)
                .take(count)
                .copied()
                .collect();
            let (
                Some(Seg2::Line {
                    a: first_start,
                    b: first_end,
                }),
                Some(Seg2::Line {
                    a: last_start,
                    b: last_end,
                }),
            ) = (segments.first().copied(), segments.get(2).copied())
            else {
                return Err(CornerTailRefusal::ModifiedTrim);
            };
            let (top, connected) = if forward {
                (
                    last_end,
                    same(first_start, from) && same(first_end, end) && same(last_start, old_end),
                )
            } else {
                (
                    first_start,
                    same(first_end, old_end) && same(last_start, end) && same(last_end, from),
                )
            };
            let past = if forward { 3 } else { count - 1 };
            let Some(Seg2::Line {
                a: past_start,
                b: past_end,
            }) = segments.get(past).copied()
            else {
                return Err(CornerTailRefusal::CurvedEdgePastTrim);
            };
            if !connected
                || !same(if forward { past_start } else { past_end }, top)
                || direction.cross(top - old_end).abs() > tolerance
                || (top - old_end).dot(direction) <= tolerance
            {
                return Err(CornerTailRefusal::ModifiedTrim);
            }
            let past_dir = past_end - past_start;
            let cross = direction.cross(past_dir);
            let original_top = (cross.abs() > 1e-12)
                .then(|| from + direction * ((past_start - from).cross(past_dir) / cross))
                .ok_or(CornerTailRefusal::ParallelEdgePastTrim)?;
            if (original_top - from).dot(direction) <= height + tolerance {
                return Err(CornerTailRefusal::ShortTrim);
            }
            if let Some(segment) = segments.get_mut(past) {
                *segment = if forward {
                    Seg2::Line {
                        a: original_top,
                        b: past_end,
                    }
                } else {
                    Seg2::Line {
                        a: past_start,
                        b: original_top,
                    }
                };
            }
            if trace {
                origins = origins
                    .iter()
                    .cycle()
                    .skip(start)
                    .take(count)
                    .copied()
                    .collect();
                if let Some(source) = origins.get_mut(past) {
                    *source = source.map(|mut ps| {
                        for p in ps.iter_mut().flatten() {
                            p.whole = false;
                        }
                        ps
                    });
                }
            }
            let side = if forward {
                Seg2::Line {
                    a: from,
                    b: original_top,
                }
            } else {
                Seg2::Line {
                    a: original_top,
                    b: from,
                }
            };
            segments.splice(0..3, [side]);
            if trace {
                // The bridge is removed and the displaced side is replaced.
                // Only the side already on the restored line survives whole.
                let consumed = Some([
                    Some(CornerTailParent {
                        segment: 0,
                        whole: forward,
                    }),
                    Some(CornerTailParent {
                        segment: 1,
                        whole: false,
                    }),
                    Some(CornerTailParent {
                        segment: 2,
                        whole: !forward,
                    }),
                ]);
                let replacement = compose(&origins, consumed);
                origins.splice(0..3, [replacement]);
            }
            restored.outer.segs = segments;
        }
    }
    let mut sides = restored.outer.segs.iter().filter_map(|segment| {
        let Seg2::Line { a, b } = *segment else {
            return None;
        };
        let top = if same(a, from) {
            b
        } else if same(b, from) {
            a
        } else {
            return None;
        };
        let rise = (top - from).dot(direction);
        (rise > height + tolerance && direction.cross(top - from).abs() <= tolerance)
            .then_some(rise)
    });
    let Some(rise) = sides.next() else {
        if restored.outer.segs.iter().any(|segment| {
            let start = segment.start();
            let finish = segment.end();
            [end, old_end].into_iter().any(|tip| {
                (same(start, tip) && (finish - tip).dot(rise_axis) > tolerance)
                    || (same(finish, tip) && (start - tip).dot(rise_axis) > tolerance)
            })
        }) {
            return Err(CornerTailRefusal::ModifiedSide);
        }
        return Ok(CornerTailWithHistory {
            region: restored,
            outer: origins,
        });
    };
    if sides.next().is_some() {
        return Err(CornerTailRefusal::AmbiguousSide);
    }
    if new_cut.abs() <= 1e-12 {
        return Ok(CornerTailWithHistory {
            region: restored,
            outer: origins,
        });
    }
    let mut result = set_back_past_impl(
        &restored, from, direction, rise, height, into, new_cut, trace,
    )
    .ok_or(CornerTailRefusal::CannotSetBack)?;
    if trace {
        result.outer = result
            .outer
            .into_iter()
            .map(|p| compose(&origins, p))
            .collect();
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(points: &[[f64; 2]]) -> Region2 {
        let segs = points
            .iter()
            .zip(points.iter().cycle().skip(1))
            .map(|(&[ax, ay], &[bx, by])| Seg2::Line {
                a: Vec2::new(ax, ay),
                b: Vec2::new(bx, by),
            })
            .collect();
        Region2 {
            outer: Loop2 { segs },
            holes: Vec::new(),
        }
    }

    #[test]
    fn corner_tail_ignores_unrelated_high_material_but_refuses_a_modified_continuation() {
        let from = Vec2::new(0.0, 0.0);
        let end = Vec2::new(0.0, 2.0);
        let run = |region: &Region2| {
            retarget_corner_tail(
                region,
                [from, end],
                from,
                Vec2::new(1.0, 0.0),
                10.0,
                0.5,
                1.0,
            )
        };
        for (points, refused) in [
            (
                [[0.0, 0.0], [0.0, 2.0], [3.0, 2.0], [3.0, 4.0], [6.0, 0.0]],
                false,
            ),
            (
                [[0.0, 0.0], [0.0, 2.0], [0.5, 2.25], [0.5, 4.0], [6.0, 0.0]],
                true,
            ),
        ] {
            let mut panel = region(&points);
            assert_eq!(
                run(&panel).err(),
                refused.then_some(CornerTailRefusal::ModifiedSide)
            );
            panel.outer = panel.outer.reversed();
            assert_eq!(
                run(&panel).err(),
                refused.then_some(CornerTailRefusal::ModifiedSide)
            );
        }
    }
    #[test]
    fn a_corner_setback_reports_retained_trimmed_and_new_boundary_runs() {
        let input = region(&[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]]);
        let out = set_back_past_with_history(
            &input,
            Vec2::new(0.0, 0.0),
            Vec2::new(0.0, 1.0),
            10.0,
            4.0,
            Vec2::new(1.0, 0.0),
            1.0,
        )
        .unwrap();
        let child = |segment| {
            Some([
                Some(CornerTailParent {
                    segment,
                    whole: false,
                }),
                None,
                None,
            ])
        };
        assert_eq!(
            out.outer,
            vec![
                Some(identity(0)),
                Some(identity(1)),
                child(2),
                child(3),
                None,
                child(3)
            ]
        );
        assert_eq!(out.region.outer.segs.len(), 6);
        assert_eq!(out.region.holes, input.holes);
    }

    #[test]
    fn restoring_a_corner_step_records_every_consumed_original_boundary() {
        let input = region(&[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]]);
        let stepped = set_back_past(
            &input,
            Vec2::new(0.0, 0.0),
            Vec2::new(0.0, 1.0),
            10.0,
            4.0,
            Vec2::new(1.0, 0.0),
            1.0,
        )
        .unwrap();
        let out = retarget_corner_tail_with_history(
            &stepped,
            [Vec2::new(0.0, 0.0), Vec2::new(0.0, 4.0)],
            Vec2::new(0.0, 0.0),
            Vec2::new(1.0, 0.0),
            10.0,
            1.0,
            0.0,
        )
        .unwrap();
        assert_eq!(
            out.outer[0],
            Some([
                Some(CornerTailParent {
                    segment: 3,
                    whole: false
                }),
                Some(CornerTailParent {
                    segment: 4,
                    whole: false
                }),
                Some(CornerTailParent {
                    segment: 5,
                    whole: true
                }),
            ])
        );
        assert_eq!(out.outer[1], Some(identity(0)));
        assert_eq!(out.outer[2], Some(identity(1)));
        assert_eq!(
            out.outer[3],
            Some([
                Some(CornerTailParent {
                    segment: 2,
                    whole: false
                }),
                None,
                None
            ])
        );
        assert_eq!(out.region.outer.segs.len(), 4);
    }
    #[test]
    fn a_three_point_setback_reverses_the_new_bridge_parent_slot_with_its_contour() {
        let input = region(&[[0.0, 0.0], [0.0, 10.0], [10.0, 0.0]]);
        let child = |segment| {
            Some([
                Some(CornerTailParent {
                    segment,
                    whole: false,
                }),
                None,
                None,
            ])
        };
        let edit = |input: &Region2| {
            set_back_past_with_history(
                input,
                Vec2::new(0.0, 0.0),
                Vec2::new(0.0, 1.0),
                10.0,
                4.0,
                Vec2::new(1.0, 0.0),
                8.0,
            )
            .unwrap()
        };
        let forward = edit(&input);
        assert_eq!(
            forward.outer,
            vec![child(0), None, child(1), Some(identity(2))]
        );
        let mut reversed = input;
        reversed.outer = reversed.outer.reversed();
        let reverse = edit(&reversed);
        assert_eq!(
            reverse.outer,
            vec![Some(identity(0)), child(1), None, child(2)]
        );
    }

    #[test]
    fn restored_parent_extent_flags_follow_both_contour_directions() {
        let input = region(&[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]]);
        let stepped = set_back_past(
            &input,
            Vec2::new(0.0, 0.0),
            Vec2::new(0.0, 1.0),
            10.0,
            4.0,
            Vec2::new(1.0, 0.0),
            1.0,
        )
        .unwrap();
        for (reverse, indices, whole) in [
            (false, [3, 4, 5], [false, false, true]),
            (true, [0, 1, 2], [true, false, false]),
        ] {
            let source = if reverse {
                Region2 {
                    outer: stepped.outer.reversed(),
                    holes: stepped.holes.clone(),
                }
            } else {
                stepped.clone()
            };
            let out = retarget_corner_tail_with_history(
                &source,
                [Vec2::new(0.0, 0.0), Vec2::new(0.0, 4.0)],
                Vec2::new(0.0, 0.0),
                Vec2::new(1.0, 0.0),
                10.0,
                1.0,
                0.0,
            )
            .unwrap();
            let ordinary = retarget_corner_tail(
                &source,
                [Vec2::new(0.0, 0.0), Vec2::new(0.0, 4.0)],
                Vec2::new(0.0, 0.0),
                Vec2::new(1.0, 0.0),
                10.0,
                1.0,
                0.0,
            )
            .unwrap();
            assert_eq!(out.region, ordinary);
            let parents = out
                .outer
                .iter()
                .flatten()
                .find(|parents| parents.iter().flatten().count() == 3)
                .unwrap();
            assert_eq!(
                *parents,
                std::array::from_fn(|i| Some(CornerTailParent {
                    segment: indices[i],
                    whole: whole[i],
                }))
            );
            let retained = if reverse { 0 } else { 5 };
            let original_side = source.outer.segs[retained];
            assert!(
                original_side
                    == Seg2::Line {
                        a: Vec2::new(0.0, 4.0),
                        b: Vec2::new(0.0, 0.0)
                    }
                    || original_side
                        == Seg2::Line {
                            a: Vec2::new(0.0, 0.0),
                            b: Vec2::new(0.0, 4.0)
                        }
            );
        }
    }
}
