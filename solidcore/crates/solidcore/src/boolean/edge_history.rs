//! Input-edge source sets observed during the existing planar boolean.
//! This changes no attributes or geometry. No recursion or global state.
//!
//! Not upstream: transient source metadata for the existing `Boolean2D` events.
//! Deviations: observer work is bounded by the existing sheet read budget;
//! exhaustion marks history unknown without changing geometry or attributes.
use std::collections::BTreeMap;

use crate::error::{EntityRef, Result};
use crate::geom::attribute::ObjectKey;
use crate::geom::context::{Context, NotifyCallback, ProofWork};
use crate::geom::object::{BoundaryEdit, NotifyOperation as N, ObjectRef};
use crate::kernel::Kernel;
use crate::primitive::primitive_creation::Boolean2d;
use crate::topology::brep::p2::copy_source_ref;
use crate::topology::loopuse::notify_ref;
use crate::topology::{edge, Brep, EdgeId};

use super::api_brep::{boolean_2d_observed, Boolean2dOutcome};

/// An opaque certificate in one operation-owned [`BoundaryHistory`].
#[derive(Clone, Debug)]
pub struct Certificate {
    index: usize,
    owner: ProofWork,
}

impl PartialEq for Certificate {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && self.owner.same_operation(&other.owner)
    }
}
impl Eq for Certificate {}
/// Coverage of one exact original input-boundary token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BoundarySource {
    /// The operation's original input-boundary token.
    pub input: usize,
    /// The certificate retains the complete original boundary.
    pub whole: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Atom {
    Root(usize),
    Part { partition: usize, right: bool },
    Unproved { uncertainty: usize },
}

/// Optional construction evidence shared by all planar-boolean recipe rounds.
/// It owns logical certificates only: object keys and database identities are
/// rebound by each individual boolean call. Exhaustion only discards evidence.
#[derive(Debug)]
pub struct BoundaryHistory {
    owner: Option<ProofWork>,
    certificates: Vec<Vec<Atom>>,
    partitions: Vec<Certificate>,
    uncertainties: Vec<Certificate>,
    roots: BTreeMap<usize, Certificate>,
}

impl Default for BoundaryHistory {
    fn default() -> Self {
        Self {
            owner: Some(ProofWork::new(crate::prism::sheet::READ_WORK)),
            certificates: Vec::new(),
            partitions: Vec::new(),
            uncertainties: Vec::new(),
            roots: BTreeMap::new(),
        }
    }
}

impl BoundaryHistory {
    // A failed observer transfer must never reset the budget or reuse logical IDs.
    /// Disabled optional metadata, without allocating an arena identity.
    pub fn untracked() -> Self {
        Self {
            owner: None,
            certificates: Vec::new(),
            partitions: Vec::new(),
            uncertainties: Vec::new(),
            roots: BTreeMap::new(),
        }
    }

    fn owns(&self, id: &Certificate) -> bool {
        self.owner
            .as_ref()
            .is_some_and(|owner| owner.same_operation(&id.owner))
    }

    /// Borrows the same operation allowance for optional query-only work.
    pub(crate) fn proof_work(&self) -> Option<ProofWork> {
        self.owner.clone()
    }

    fn is_exhausted(&self) -> bool {
        self.owner.as_ref().is_none_or(ProofWork::is_disabled)
    }

    fn charge(&mut self, count: usize) -> bool {
        self.owner
            .as_ref()
            .is_some_and(|work| work.charge(u64::try_from(count).ok()))
    }

    fn copy_atoms(&mut self, id: &Certificate) -> Option<Vec<Atom>> {
        if !self.owns(id) {
            return None;
        }
        self.copy_index_atoms(id.index)
    }

    // Indices are local to this arena. Callers must validate ownership before
    // deduplicating or looking up a Certificate supplied from another operation.
    fn copy_index_atoms(&mut self, index: usize) -> Option<Vec<Atom>> {
        let count = self.certificates.get(index)?.len();
        if !self.charge(count.checked_add(1)?) {
            return None;
        }
        self.certificates.get(index).cloned()
    }

    fn store(&mut self, atoms: Vec<Atom>) -> Option<Certificate> {
        if atoms.is_empty() || !self.charge(1) {
            return None;
        }
        let id = Certificate {
            index: self.certificates.len(),
            owner: self.owner.as_ref()?.clone(),
        };
        self.certificates.push(atoms);
        Some(id)
    }

    // Comparisons/moves are charged before they happen; no guessed std-sort bound.
    fn sort(&mut self, atoms: &mut Vec<Atom>) -> Option<()> {
        for i in 1..atoms.len() {
            let mut j = i;
            while j > 0 {
                if !self.charge(1) {
                    return None;
                }
                if atoms.get(j - 1)? <= atoms.get(j)? {
                    break;
                }
                if !self.charge(2) {
                    return None;
                }
                let (left, right) = atoms.split_at_mut_checked(j)?;
                std::mem::swap(left.last_mut()?, right.first_mut()?);
                j -= 1;
            }
        }
        // At most one comparison and two compaction moves per input atom.
        if !self.charge(atoms.len().checked_mul(3)?) {
            return None;
        }
        atoms.dedup();
        Some(())
    }

    fn contains(&mut self, atoms: &[Atom], needle: Atom) -> Option<bool> {
        let (mut lo, mut hi) = (0, atoms.len());
        while lo < hi {
            if !self.charge(1) {
                return None;
            }
            let mid = lo + (hi - lo) / 2;
            match atoms.get(mid)?.cmp(&needle) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(true),
            }
        }
        Some(false)
    }

    // A search visits at most every stored key. Retain the conservative two-scalar
    // comparison envelope and one insertion/end bookkeeping item per operation.
    fn charge_tree(&mut self, entries: usize, operations: usize) -> bool {
        entries
            .checked_add(1)
            .and_then(|n| n.checked_mul(2))
            .and_then(|n| n.checked_mul(operations))
            .is_some_and(|n| self.charge(n))
    }

    fn normalize(&mut self, mut atoms: Vec<Atom>) -> Option<Certificate> {
        self.sort(&mut atoms)?;
        // A successful producer partition is an exact disjoint cover. Only its
        // two distinct children, never duplicate copies, recover the parent.
        let mut changed = true;
        while changed {
            changed = false;
            for partition in (0..self.partitions.len()).rev() {
                if !self.charge(1) {
                    return None;
                }
                let left = Atom::Part {
                    partition,
                    right: false,
                };
                let right = Atom::Part {
                    partition,
                    right: true,
                };
                if self.contains(&atoms, left)? && self.contains(&atoms, right)? {
                    let parent = self.partitions.get(partition)?.clone();
                    let parent_atoms = self.copy_atoms(&parent)?;
                    // Two child comparisons and at most one compaction move per
                    // retained input, followed by the parent-atom extension.
                    let work = atoms
                        .len()
                        .checked_mul(3)?
                        .checked_add(parent_atoms.len())?;
                    if !self.charge(work) {
                        return None;
                    }
                    atoms.retain(|atom| *atom != left && *atom != right);
                    atoms.extend(parent_atoms);
                    self.sort(&mut atoms)?;
                    changed = true;
                }
            }
        }
        self.store(atoms)
    }

    /// Certifies one original whole constructor boundary, seeded exactly once.
    pub fn original(&mut self, input: usize) -> Option<Certificate> {
        if self.is_exhausted() {
            return None;
        }
        if !self.charge_tree(self.roots.len(), 1) {
            return None;
        }
        if let Some(id) = self.roots.get(&input).cloned() {
            return Some(id);
        }
        if !self.charge(2) {
            return None;
        }
        let id = self.store(vec![Atom::Root(input)])?;
        if !self.charge_tree(self.roots.len(), 1) {
            return None;
        }
        self.roots.insert(input, id.clone());
        Some(id)
    }

    /// Unites complete current-edge construction certificates. Unknown is not
    /// the empty set, and two copies never count as two partition children.
    pub fn united(
        &mut self,
        a: Option<Certificate>,
        b: Option<Certificate>,
    ) -> Option<Certificate> {
        let (a, b) = (a?, b?);
        if !self.owns(&a) || !self.owns(&b) {
            return None;
        }
        if a == b {
            return (!self.is_exhausted()).then_some(a);
        }
        let mut atoms = self.copy_atoms(&a)?;
        let other = self.copy_atoms(&b)?;
        if !self.charge(other.len()) {
            return None;
        }
        atoms.extend(other);
        self.normalize(atoms)
    }

    // Called only by a complete successful producer partition, not by generic
    // coincidence, a count, a geometric match, or the existence of descendants.
    /// Records coarse ancestry that proves neither containment nor contribution.
    /// It cannot project to a known singleton or combine with copies into a whole.
    pub fn uncertain(&mut self, parent: Option<Certificate>) -> Option<Certificate> {
        let parent = parent?;
        if !self.owns(&parent) || !self.charge(3) {
            return None;
        }
        let uncertainty = self.uncertainties.len();
        self.uncertainties.push(parent);
        self.store(vec![Atom::Unproved { uncertainty }])
    }

    fn partition(&mut self, parent: Certificate) -> Option<[Certificate; 2]> {
        if !self.owns(&parent) {
            return None;
        }
        self.certificates.get(parent.index)?;
        if !self.charge(5) {
            return None;
        }
        let partition = self.partitions.len();
        self.partitions.push(parent);
        let left = self.store(vec![Atom::Part {
            partition,
            right: false,
        }])?;
        let right = self.store(vec![Atom::Part {
            partition,
            right: true,
        }])?;
        Some([left, right])
    }

    fn covered_by(&mut self, witness: &[Atom], certificate: &Certificate) -> Option<bool> {
        if !self.owns(certificate) || !self.charge(3) {
            return None;
        }
        let mut todo = vec![certificate.clone()];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(id) = todo.pop() {
            if !self.charge_tree(seen.len(), 1) {
                return None;
            }
            if !self.owns(&id) {
                return None;
            }
            if !seen.insert(id.index) {
                continue;
            }
            let atoms = self.copy_atoms(&id)?;
            for atom in atoms {
                if !self.charge(1) {
                    return None;
                }
                let parent = match atom {
                    Atom::Part { partition, .. } if !self.contains(witness, atom)? => {
                        self.partitions.get(partition).cloned()
                    }
                    Atom::Root(_) | Atom::Part { .. } | Atom::Unproved { .. } => None,
                };
                if let Some(parent) = parent {
                    if !self.charge(1) {
                        return None;
                    }
                    todo.push(parent);
                }
            }
        }
        // All references point to older certificate indices. BTreeSet already
        // orders those indices in this same arena, without a second sort/copy.
        let mut covered = BTreeMap::new();
        for key in seen {
            let atoms = self.copy_index_atoms(key)?;
            let mut yes = true;
            for atom in atoms {
                if !self.charge(1) {
                    return None;
                }
                let holds = match atom {
                    Atom::Unproved { .. } => false,
                    Atom::Root(_) => self.contains(witness, atom)?,
                    Atom::Part { partition, .. } => {
                        if self.contains(witness, atom)? {
                            true
                        } else {
                            if !self.charge_tree(covered.len(), 1) {
                                return None;
                            }
                            self.partitions
                                .get(partition)
                                .filter(|parent| self.owns(parent))
                                .and_then(|parent| covered.get(&parent.index))
                                .copied()
                                .unwrap_or(false)
                        }
                    }
                };
                if !holds {
                    yes = false;
                    break;
                }
            }
            if !self.charge_tree(covered.len(), 1) {
                return None;
            }
            covered.insert(key, yes);
        }
        if !self.charge_tree(covered.len(), 1) {
            return None;
        }
        covered.get(&certificate.index).copied()
    }

    fn roots_of(&mut self, certificate: Certificate) -> Option<Vec<usize>> {
        if !self.owns(&certificate) || !self.charge(2) {
            return None;
        }
        let mut todo = vec![certificate];
        let mut visited = std::collections::BTreeSet::new();
        let mut roots = std::collections::BTreeSet::new();
        while let Some(id) = todo.pop() {
            if !self.charge(1) {
                return None;
            }
            if !self.charge_tree(visited.len(), 1) {
                return None;
            }
            if !self.owns(&id) {
                return None;
            }
            if !visited.insert(id.index) {
                continue;
            }
            let atoms = self.copy_atoms(&id)?;
            if !self.charge(atoms.len().checked_mul(3)?) {
                return None;
            }
            for atom in atoms {
                match atom {
                    Atom::Root(input) => {
                        if !self.charge_tree(roots.len(), 1) {
                            return None;
                        }
                        roots.insert(input);
                    }
                    Atom::Part { partition, .. } => {
                        todo.push(self.partitions.get(partition)?.clone());
                    }
                    Atom::Unproved { .. } => return None,
                }
            }
        }
        if !self.charge(roots.len()) {
            return None;
        }
        Some(roots.into_iter().collect())
    }

    /// Projects retained producer evidence to complete input-token coverage.
    /// Partial pieces of a mixed parent remain unknown until a complete rejoin.
    pub fn sources(&mut self, id: &Certificate) -> Option<Vec<BoundarySource>> {
        let atoms = self.copy_atoms(id)?;
        let mut result = BTreeMap::<usize, bool>::new();
        for atom in &atoms {
            if !self.charge(1) {
                return None;
            }
            match *atom {
                Atom::Root(input) => {
                    if !self.charge_tree(result.len(), 1) {
                        return None;
                    }
                    result.insert(input, true);
                }
                // Generic ancestry carries no finite-domain proof, even if a
                // numerically identical root occurs elsewhere in this certificate.
                Atom::Unproved { .. } => return None,
                Atom::Part { partition, .. } => {
                    let parent = self.partitions.get(partition)?.clone();
                    // A complete parent already present makes a copied partial
                    // descendant redundant, without manufacturing coverage.
                    if self.covered_by(&atoms, &parent)? {
                        continue;
                    }
                    let roots = self.roots_of(parent)?;
                    let [input] = roots.as_slice() else {
                        return None;
                    };
                    if !self.charge_tree(result.len(), 1) {
                        return None;
                    }
                    result.entry(*input).or_insert(false);
                }
            }
        }
        if result.is_empty() || !self.charge(result.len()) {
            return None;
        }
        Some(
            result
                .into_iter()
                .map(|(input, whole)| BoundarySource { input, whole })
                .collect(),
        )
    }
}

/// Optional logical certificates bound to actual live edges of one working sheet.
/// Their arena is owned by the enclosing planar operation, never the object-key map.
pub type EdgeSources = BTreeMap<EdgeId, Option<Certificate>>;

#[derive(Debug)]
enum Pending {
    Split {
        parent: ObjectKey,
        lower: ObjectKey,
        upper: ObjectKey,
        bounds: [f64; 2],
        source: Option<Certificate>,
    },
    Join {
        survivor: ObjectKey,
        removed: ObjectKey,
        first: Option<Certificate>,
        second: Option<Certificate>,
    },
    Alias {
        first: ObjectKey,
        second: ObjectKey,
        sources: [Option<Certificate>; 2],
        dependencies: [Option<ObjectKey>; 2],
        copies: u8,
        valid: bool,
    },
}

impl Pending {
    fn contains(&self, key: ObjectKey) -> bool {
        match self {
            Self::Split {
                parent,
                lower,
                upper,
                ..
            } => [*parent, *lower, *upper].contains(&key),
            Self::Join {
                survivor, removed, ..
            } => [*survivor, *removed].contains(&key),
            Self::Alias { .. } => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FreshBirth {
    AwaitCurve,
    Initialized,
}

#[derive(Debug)]
struct Collector {
    // These keys exist only inside this single current Context/boolean call.
    sources: BTreeMap<ObjectKey, Option<Certificate>>,
    history: BoundaryHistory,
    deferred: BTreeMap<ObjectKey, ObjectKey>,
    births: BTreeMap<ObjectKey, FreshBirth>,
    pending: Vec<Option<Pending>>,
    incomplete: bool,
}

impl Collector {
    fn value(&mut self, key: ObjectKey) -> Option<Certificate> {
        if !self.history.charge_tree(self.sources.len(), 1) {
            return None;
        }
        self.sources.get(&key).cloned().flatten()
    }

    fn protected(&mut self, key: ObjectKey) -> bool {
        // One optional-slot test and at most three edge-key comparisons.
        if !self
            .pending
            .len()
            .checked_mul(4)
            .is_some_and(|work| self.history.charge(work))
        {
            return true;
        }
        self.pending.iter().flatten().any(|edit| edit.contains(key))
    }

    fn join(&mut self, target: ObjectKey, incoming: Option<Certificate>) {
        if !self.history.charge_tree(self.sources.len(), 2) {
            return;
        }
        let previous = self.sources.get(&target).cloned();
        let merged = match previous {
            None => incoming,
            Some(old) => self.history.united(old, incoming),
        };
        self.sources.insert(target, merged);
    }

    fn changed_extent(&mut self, key: ObjectKey) {
        if !self.protected(key) {
            let old = self.value(key);
            let uncertain = self.history.uncertain(old);
            if self.history.charge_tree(self.sources.len(), 1) {
                self.sources.insert(key, uncertain);
            }
        }
    }

    fn edge(r: Option<ObjectRef>) -> Option<ObjectKey> {
        r.filter(|r| r.type_code == edge::TYPE).map(|r| r.key)
    }

    fn begin_split(
        &mut self,
        parent: ObjectRef,
        lower: ObjectRef,
        upper: ObjectRef,
        bounds: [f64; 2],
        at: f64,
    ) {
        let (Some(parent), Some(lower), Some(upper)) = (
            Self::edge(Some(parent)),
            Self::edge(Some(lower)),
            Self::edge(Some(upper)),
        ) else {
            self.incomplete = true;
            return;
        };
        let source = self.value(parent);
        if !(lower != upper
            && bounds.into_iter().chain([at]).all(f64::is_finite)
            && bounds[0] < at
            && at < bounds[1]
            && self.history.charge(1))
        {
            self.incomplete = true;
            return;
        }
        if !self.reserve_pending_slot() {
            return;
        }
        self.pending.push(Some(Pending::Split {
            parent,
            lower,
            upper,
            bounds,
            source,
        }));
    }

    fn reserve_pending_slot(&mut self) -> bool {
        // The same canonical growth/move bound used for an alias bracket also
        // covers split/join slots. Reserve before constructing any queued record.
        let cost = self
            .pending
            .len()
            .checked_add(1)
            .and_then(|n| n.checked_mul(384));
        if !cost.is_some_and(|work| self.history.charge(work)) {
            return false;
        }
        if self.pending.try_reserve_exact(1).is_err() {
            if let Some(work) = self.history.proof_work() {
                work.disable();
            }
            return false;
        }
        true
    }

    fn take_pending(&mut self, at: usize) -> Option<Pending> {
        // Every slot is checked once and may move once. The existing canonical
        // 384-byte bracket envelope bounds its retained record and move.
        if !self
            .pending
            .len()
            .checked_mul(386)
            .is_some_and(|work| self.history.charge(work))
        {
            return None;
        }
        let taken = self.pending.get_mut(at)?.take();
        self.pending.retain(Option::is_some);
        taken
    }

    fn commit_split(
        &mut self,
        parent: ObjectRef,
        copy_source: crate::nurbs::b_spline_curve_n_lib::CopySourceOutcome,
        lower_source: crate::nurbs::b_spline_curve_n_lib::TrimSourceOutcome,
        upper_source: crate::nurbs::b_spline_curve_n_lib::TrimSourceOutcome,
    ) {
        let Some(parent) = Self::edge(Some(parent)) else {
            self.incomplete = true;
            return;
        };
        // Optional/variant tests and one parent-key comparison per slot.
        if !self
            .pending
            .len()
            .checked_mul(3)
            .is_some_and(|work| self.history.charge(work))
        {
            return;
        }
        let Some(at_pending) = self
            .pending
            .iter()
            .rposition(|p| matches!(p, Some(Pending::Split { parent: p, .. }) if *p == parent))
        else {
            self.incomplete = true;
            return;
        };
        // rposition proves the remove index is present.
        let Some(pending) = self.take_pending(at_pending) else {
            self.incomplete = true;
            return;
        };
        let Pending::Split {
            lower,
            upper,
            bounds,
            source,
            ..
        } = pending
        else {
            self.incomplete = true;
            return;
        };
        let exact_partition = match (copy_source, lower_source, upper_source) {
            (
                crate::nurbs::b_spline_curve_n_lib::CopySourceOutcome::Preserved,
                crate::nurbs::b_spline_curve_n_lib::TrimSourceOutcome::Retained(lower),
                crate::nurbs::b_spline_curve_n_lib::TrimSourceOutcome::Retained(upper),
            ) => {
                let endpoints = [lower.min(), lower.max(), upper.min(), upper.max()];
                endpoints.into_iter().all(f64::is_finite)
                    && lower.min() == bounds[0]
                    && upper.max() == bounds[1]
                    && lower.max() == upper.min()
                    && bounds[0] < lower.max()
                    && lower.max() < bounds[1]
            }
            _ => false,
        };
        let children = if exact_partition {
            source.and_then(|s| self.history.partition(s))
        } else {
            None
        };
        let (left, right) = match children {
            Some([a, b]) => (Some(a), Some(b)),
            None => (None, None),
        };
        if self
            .history
            .charge_tree(self.sources.len().saturating_add(1), 2)
        {
            self.sources.insert(lower, left);
            self.sources.insert(upper, right);
        }
    }

    fn begin_join(&mut self, survivor: ObjectRef, removed: ObjectRef) {
        let (Some(survivor), Some(removed)) =
            (Self::edge(Some(survivor)), Self::edge(Some(removed)))
        else {
            self.incomplete = true;
            return;
        };
        let first = self.value(survivor);
        let second = self.value(removed);
        if survivor == removed || !self.history.charge(1) {
            self.incomplete = true;
            return;
        }
        if !self.reserve_pending_slot() {
            return;
        }
        self.pending.push(Some(Pending::Join {
            survivor,
            removed,
            first,
            second,
        }));
    }

    fn commit_join(&mut self, survivor: ObjectRef, removed: ObjectRef) {
        let (Some(survivor), Some(removed)) =
            (Self::edge(Some(survivor)), Self::edge(Some(removed)))
        else {
            self.incomplete = true;
            return;
        };
        // Optional/variant tests and up to two edge-key comparisons per slot.
        if !self
            .pending
            .len()
            .checked_mul(4)
            .is_some_and(|work| self.history.charge(work))
        {
            return;
        }
        let Some(at) = self.pending.iter().rposition(|p| matches!(p, Some(Pending::Join { survivor: a, removed: b, .. }) if *a == survivor && *b == removed)) else { self.incomplete = true; return };
        // rposition proves the remove index is present.
        let Some(Pending::Join { first, second, .. }) = self.take_pending(at) else {
            self.incomplete = true;
            return;
        };
        let joined = self.history.united(first, second);
        if self
            .history
            .charge_tree(self.sources.len().saturating_add(1), 2)
        {
            self.sources.insert(survivor, joined);
            self.sources.remove(&removed);
        }
    }

    fn verified_alias(&mut self, first: ObjectRef, second: ObjectRef) {
        let (Some(first), Some(second)) = (Self::edge(Some(first)), Self::edge(Some(second)))
        else {
            self.incomplete = true;
            return;
        };
        let a = self.value(first);
        let b = self.value(second);
        let alias = self.history.united(a, b);
        if self
            .history
            .charge_tree(self.sources.len().saturating_add(1), 2)
        {
            self.sources.insert(first, alias.clone());
            self.sources.insert(second, alias);
        }
    }

    fn verified_copy(&mut self, source: ObjectRef, target: ObjectRef) {
        let (Some(source), Some(target)) = (Self::edge(Some(source)), Self::edge(Some(target)))
        else {
            self.incomplete = true;
            return;
        };
        if self.protected(source) {
            if self.history.charge_tree(self.sources.len(), 1) {
                self.sources.insert(target, None);
            }
            return;
        }
        let value = self.value(source);
        // A typed constructor copy owns the NEW target's entire boundary;
        // coarse creation notifications are not additional contributors.
        if self.history.charge_tree(self.sources.len(), 1) {
            self.sources.insert(target, value);
        }
    }

    fn invalidate_dependencies(&mut self, key: ObjectKey) {
        if !self.history.charge_tree(self.births.len(), 1)
            || !self.history.charge_tree(self.sources.len(), 1)
        {
            return;
        }
        if self.births.remove(&key).is_some() {
            self.sources.insert(key, None);
        }
        // Each removed dependency can also seed one explicit unknown target.
        let n = self.deferred.len();
        if !self
            .history
            .charge_tree(self.sources.len().saturating_add(n), n)
            || !n.checked_mul(3).is_some_and(|w| self.history.charge(w))
        {
            return;
        }
        let sources = &mut self.sources;
        self.deferred.retain(|target, opposite| {
            let keep = *target != key && *opposite != key;
            if !keep {
                sources.insert(*target, None);
            }
            keep
        });
        if !self
            .pending
            .len()
            .checked_mul(3)
            .is_some_and(|w| self.history.charge(w))
        {
            return;
        }
        for pending in self.pending.iter_mut().flatten() {
            if let Pending::Alias {
                first,
                second,
                valid,
                ..
            } = pending
            {
                if *first == key || *second == key {
                    *valid = false;
                }
            }
        }
    }

    fn fresh_allocated(&mut self, target: ObjectRef) {
        let Some(target) = Self::edge(Some(target)) else {
            return;
        };
        if !self.history.charge_tree(self.sources.len(), 1)
            || !self.history.charge_tree(self.deferred.len(), 1)
            || !self.history.charge_tree(self.births.len(), 2)
            || !self.history.charge(512)
        {
            return;
        }
        if !self.sources.contains_key(&target)
            && !self.deferred.contains_key(&target)
            && !self.births.contains_key(&target)
        {
            self.births.insert(target, FreshBirth::AwaitCurve);
        }
    }

    fn initialize_fresh_curve(&mut self, key: ObjectKey) -> bool {
        if !self.history.charge_tree(self.births.len(), 1) {
            return false;
        }
        let Some(state) = self.births.get_mut(&key) else {
            return false;
        };
        if *state != FreshBirth::AwaitCurve {
            return false;
        }
        *state = FreshBirth::Initialized;
        true
    }

    fn fresh_dependency(&mut self, target: ObjectRef, opposite: ObjectRef) {
        let (Some(target), Some(opposite)) = (Self::edge(Some(target)), Self::edge(Some(opposite)))
        else {
            self.incomplete = true;
            return;
        };
        if target == opposite
            || !self.history.charge_tree(self.sources.len(), 1)
            || !self.history.charge_tree(self.deferred.len(), 2)
            || !self.history.charge_tree(self.births.len(), 1)
            || !self.history.charge(512)
        {
            return;
        }
        // Absence alone is not evidence: ONLY the typed fresh constructor can
        // enter here. An existing None/uncertain contributor is never repaired.
        if self.births.remove(&target) == Some(FreshBirth::Initialized)
            && !self.sources.contains_key(&target)
            && !self.deferred.contains_key(&target)
        {
            self.deferred.insert(target, opposite);
        }
    }

    fn begin_alias(&mut self, first: ObjectRef, second: ObjectRef) {
        let (Some(first), Some(second)) = (Self::edge(Some(first)), Self::edge(Some(second)))
        else {
            self.incomplete = true;
            return;
        };
        if first == second
            || !self
                .pending
                .len()
                .checked_mul(2)
                .is_some_and(|w| self.history.charge(w))
        {
            self.incomplete = true;
            return;
        }
        if self
            .pending
            .iter()
            .flatten()
            .any(|p| matches!(p, Pending::Alias { .. }))
        {
            self.incomplete = true;
            return;
        }
        // This executes on delivery, after every preceding queued producer edit.
        let sources = [self.value(first), self.value(second)];
        if !self.history.charge_tree(self.deferred.len(), 2) {
            return;
        }
        let dependencies = [
            self.deferred.get(&first).copied(),
            self.deferred.get(&second).copied(),
        ];
        if !self.reserve_pending_slot() {
            return;
        }
        self.pending.push(Some(Pending::Alias {
            first,
            second,
            sources,
            dependencies,
            copies: 0,
            valid: true,
        }));
    }

    fn alias_copy(&mut self, source: ObjectKey, target: ObjectKey) -> bool {
        if !self
            .pending
            .len()
            .checked_mul(6)
            .is_some_and(|w| self.history.charge(w))
        {
            return false;
        }
        let mut owned = false;
        for pending in self.pending.iter_mut().flatten() {
            if let Pending::Alias {
                first,
                second,
                copies,
                valid,
                ..
            } = pending
            {
                let expected = match *copies {
                    0 => source == *second && target == *first,
                    1 => source == *first && target == *second,
                    _ => false,
                };
                if *valid && expected {
                    *copies += 1;
                    owned = true;
                } else {
                    *valid = false;
                }
            }
        }
        owned
    }

    fn commit_alias(&mut self, first: ObjectRef, second: ObjectRef) {
        let (Some(first), Some(second)) = (Self::edge(Some(first)), Self::edge(Some(second)))
        else {
            self.incomplete = true;
            return;
        };
        if !self
            .pending
            .len()
            .checked_mul(4)
            .is_some_and(|w| self.history.charge(w))
        {
            return;
        }
        let Some(at) = self.pending.iter().rposition(|p| matches!(p, Some(Pending::Alias { first: a, second: b, .. }) if *a == first && *b == second)) else {
            self.incomplete = true;
            return;
        };
        let Some(Pending::Alias {
            sources: [a, b],
            dependencies: [da, db],
            copies,
            valid,
            ..
        }) = self.take_pending(at)
        else {
            self.incomplete = true;
            return;
        };
        if !valid || copies != 2 || self.incomplete {
            return;
        }
        // At most ONE deferred side may borrow its actual named opposite's
        // CURRENT snapshot. None and two mutually deferred sides stay unknown.
        let first_value = a
            .clone()
            .or_else(|| (da == Some(second)).then(|| b.clone()).flatten());
        let second_value = b.or_else(|| (db == Some(first)).then_some(a).flatten());
        let alias = self.history.united(first_value, second_value);
        if self
            .history
            .charge_tree(self.sources.len().saturating_add(1), 2)
            && self.history.charge_tree(self.deferred.len(), 2)
        {
            self.sources.insert(first, alias.clone());
            self.sources.insert(second, alias);
            self.deferred.remove(&first);
            self.deferred.remove(&second);
        }
    }

    fn prepare_notification(
        &mut self,
        operation: N,
        caller: ObjectRef,
        first: Option<ObjectRef>,
        second: Option<ObjectRef>,
        third: Option<ObjectRef>,
    ) -> bool {
        // Only the exact two bracket-owned generic Copies are exempt from
        // dependency invalidation. Their ordinary metadata effects still run.
        // Only one producer-bracketed installation on the actual newly allocated
        // edge may precede its final dependency. No generic None is reclassified.
        let initializing = operation == N::ChangeGeometry
            && caller.type_code == edge::TYPE
            && first.is_some()
            && second.is_none()
            && third.is_none()
            && self.initialize_fresh_curve(caller.key);
        let own_copy = operation == N::Copy
            && caller.type_code == edge::TYPE
            && second.is_none()
            && third.is_none()
            && Self::edge(first).is_some_and(|target| self.alias_copy(caller.key, target));
        if !own_copy && !initializing {
            match operation {
                N::Copy if caller.type_code == edge::TYPE => {
                    if let Some(target) = Self::edge(first) {
                        self.invalidate_dependencies(target);
                    }
                }
                N::PreEdit | N::ChangeGeometry | N::Destruction
                    if caller.type_code == edge::TYPE =>
                {
                    self.invalidate_dependencies(caller.key);
                }
                N::SplitInBrep | N::MergeInBrep | N::Coincident => {
                    for key in [first, second, third].into_iter().filter_map(Self::edge) {
                        self.invalidate_dependencies(key);
                    }
                }
                N::Split | N::Merge => {
                    if let Some(key) = Self::edge(third) {
                        self.invalidate_dependencies(key);
                    }
                }
                N::TrimNoSplitInBrep => {
                    if let Some(key) = Self::edge(first) {
                        self.invalidate_dependencies(key);
                    }
                }
                _ => {}
            }
        }
        initializing
    }

    fn edit(&mut self, event: BoundaryEdit) {
        if !self.history.charge(1) {
            return;
        }
        match event {
            BoundaryEdit::UncertifiedExtentChanged { edge: current } => {
                let Some(key) = Self::edge(Some(current)) else {
                    self.incomplete = true;
                    return;
                };
                self.invalidate_dependencies(key);
                if self.protected(key) {
                    // This edit is outside a typed producer's coverage contract.
                    // An older pending snapshot cannot certify it on commit.
                    self.incomplete = true;
                } else {
                    self.changed_extent(key);
                }
            }
            BoundaryEdit::FreshAllocated { target } => self.fresh_allocated(target),
            BoundaryEdit::FreshDependency { target, opposite } => {
                self.fresh_dependency(target, opposite);
            }
            BoundaryEdit::BeginAlias { first, second } => self.begin_alias(first, second),
            BoundaryEdit::CommitAlias { first, second } => self.commit_alias(first, second),
            BoundaryEdit::BeginSplit {
                parent,
                lower,
                upper,
                bounds,
                at,
            } => {
                for r in [parent, lower, upper] {
                    self.invalidate_dependencies(r.key);
                }
                self.begin_split(parent, lower, upper, bounds, at);
            }
            BoundaryEdit::CommitSplit {
                parent,
                copy_source,
                lower_source,
                upper_source,
            } => {
                self.invalidate_dependencies(parent.key);
                self.commit_split(parent, copy_source, lower_source, upper_source);
            }
            BoundaryEdit::BeginJoin { survivor, removed } => {
                self.invalidate_dependencies(survivor.key);
                self.invalidate_dependencies(removed.key);
                self.begin_join(survivor, removed);
            }
            BoundaryEdit::CommitJoin { survivor, removed } => {
                self.invalidate_dependencies(survivor.key);
                self.invalidate_dependencies(removed.key);
                self.commit_join(survivor, removed);
            }
            BoundaryEdit::Aliased { first, second } => {
                self.invalidate_dependencies(first.key);
                self.invalidate_dependencies(second.key);
                self.verified_alias(first, second);
            }
            BoundaryEdit::Copied { source, target } => {
                self.invalidate_dependencies(target.key);
                self.verified_copy(source, target);
            }
        }
    }
}

impl NotifyCallback for Collector {
    fn execute(
        &mut self,
        operation: N,
        caller: ObjectRef,
        first: Option<ObjectRef>,
        second: Option<ObjectRef>,
        third: Option<ObjectRef>,
    ) -> Result<()> {
        if !self.history.charge(1) {
            return Ok(());
        }
        let initializing = self.prepare_notification(operation, caller, first, second, third);
        match operation {
            N::Copy if caller.type_code == edge::TYPE => {
                if let Some(target) = Self::edge(first) {
                    // Generic Copy is also used for alias attribute propagation.
                    // Only a later typed producer can certify finite coverage.
                    let source = self.value(caller.key);
                    let ancestry = self.history.uncertain(source);
                    self.join(target, ancestry);
                } else {
                    self.incomplete = true;
                }
            }
            N::SplitInBrep => {
                if let (Some(parent), Some(left), Some(right)) =
                    (Self::edge(first), Self::edge(second), Self::edge(third))
                {
                    if !self.protected(parent) {
                        let source = self.value(parent);
                        let ancestry = self.history.uncertain(source);
                        if self
                            .history
                            .charge_tree(self.sources.len().saturating_add(1), 2)
                        {
                            self.sources.insert(left, ancestry.clone());
                            self.sources.insert(right, ancestry);
                        }
                    }
                }
            }
            N::MergeInBrep => {
                if let (Some(target), Some(source)) = (Self::edge(first), Self::edge(second)) {
                    if !self.protected(target) {
                        let source = self.value(source);
                        let ancestry = self.history.uncertain(source);
                        self.join(target, ancestry);
                    }
                }
            }
            N::Coincident => {
                if let (Some(a), Some(b)) = (Self::edge(first), Self::edge(second)) {
                    let av = self.value(a);
                    let bv = self.value(b);
                    let unproved_a = self.history.uncertain(av);
                    let unproved_b = self.history.uncertain(bv);
                    self.join(a, unproved_b);
                    self.join(b, unproved_a);
                }
            }
            N::ChangeGeometry if caller.type_code == edge::TYPE && !initializing => {
                self.changed_extent(caller.key);
            }
            N::Split | N::Merge => {
                if let Some(owner) = Self::edge(third) {
                    self.changed_extent(owner);
                }
            }
            N::TrimNoSplitInBrep => {
                if let Some(target) = Self::edge(first) {
                    self.changed_extent(target);
                }
            }
            N::Destruction if caller.type_code == edge::TYPE => {
                if self.history.charge_tree(self.sources.len(), 1) {
                    self.sources.remove(&caller.key);
                }
            }
            N::Unknown if caller.type_code == edge::TYPE => self.incomplete = true,
            _ => {}
        }
        Ok(())
    }
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn core::any::Any> {
        Some(self)
    }
    fn reserve_boundary_edit(&mut self, logical_work: usize) -> bool {
        self.history.charge(logical_work)
    }
    fn boundary_edit(&mut self, event: BoundaryEdit) {
        self.edit(event);
    }
}

pub(super) fn run(
    k: &mut Kernel,
    a: Brep,
    b: Brep,
    op: Boolean2d,
    first: &EdgeSources,
    second: &EdgeSources,
    history: &mut BoundaryHistory,
) -> Result<(Boolean2dOutcome, EdgeSources)> {
    // If extraction ever fails, leave a permanently disabled sentinel. A fresh
    // arena must never reinterpret intermediate handles or reset the allowance.
    let mut collector = Collector {
        sources: BTreeMap::new(),
        history: core::mem::replace(history, BoundaryHistory::untracked()),
        deferred: BTreeMap::new(),
        births: BTreeMap::new(),
        pending: Vec::new(),
        incomplete: false,
    };
    for (body, sources, key) in [
        (&a, first, notify_ref as fn(EntityRef) -> ObjectRef),
        (&b, second, copy_source_ref as fn(EntityRef) -> ObjectRef),
    ] {
        // edges(body, None) copies exactly this list; metadata needs no copy.
        for &edge in body.edge_list() {
            if !collector.history.charge(1)
                || !collector.history.charge_tree(sources.len(), 1)
                || !collector.history.charge_tree(collector.sources.len(), 1)
            {
                break;
            }
            let value = sources
                .get(&edge)
                .cloned()
                .flatten()
                .filter(|id| collector.history.owns(id));
            collector
                .sources
                .insert(key(EntityRef::Edge(edge)).key, value);
        }
    }
    let mut context = Context::default();
    context.set_proof_work(collector.history.proof_work());
    context.set_user_notify_callback(Some(Box::new(collector)));
    let result = boolean_2d_observed(k, a, b, op, Some(&mut context));
    let mut sources = EdgeSources::new();
    if let Some(observed) = context
        .user_notify_callback_mut()
        .and_then(|c| c.as_any_mut())
        .and_then(|c| c.downcast_mut::<Collector>())
    {
        let complete = !observed.incomplete
            && observed.history.charge(observed.pending.len())
            && !observed.pending.iter().any(Option::is_some)
            && !observed.history.is_exhausted();
        if let Ok(Boolean2dOutcome::Result(body)) = &result {
            for &edge in body.edge_list() {
                if !observed.history.charge(1)
                    || (complete && !observed.history.charge_tree(observed.sources.len(), 1))
                    || !observed.history.charge_tree(sources.len(), 1)
                {
                    break;
                }
                let value = if complete {
                    observed
                        .sources
                        .get(&notify_ref(EntityRef::Edge(edge)).key)
                        .cloned()
                        .flatten()
                } else {
                    None
                };
                sources.insert(edge, value);
            }
        }
        if result.is_err() {
            if let Some(work) = observed.history.proof_work() {
                work.disable();
            }
        }
        *history = core::mem::replace(&mut observed.history, BoundaryHistory::untracked());
    } else if let Some(work) = context.proof_work() {
        work.disable();
    }
    Ok((result?, sources))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::float_cmp)]
    use super::*;

    #[test]
    fn copies_do_not_replace_a_missing_partition_sibling() {
        let mut h = BoundaryHistory::default();
        let root = h.original(7).unwrap();
        let [left, right] = h.partition(root).unwrap();
        let copied_left = h.united(Some(left.clone()), Some(left)).unwrap();
        assert_eq!(
            h.sources(&copied_left),
            Some(vec![BoundarySource {
                input: 7,
                whole: false
            }])
        );
        let whole = h.united(Some(copied_left), Some(right)).unwrap();
        assert_eq!(
            h.sources(&whole),
            Some(vec![BoundarySource {
                input: 7,
                whole: true
            }])
        );
    }

    #[test]
    fn nested_partitions_rejoin_to_the_complete_mixed_parent() {
        let mut h = BoundaryHistory::default();
        let a = h.original(0).unwrap();
        let b = h.original(1).unwrap();
        let parent = h.united(Some(a), Some(b)).unwrap();
        let [left, right] = h.partition(parent.clone()).unwrap();
        assert_eq!(h.sources(&left), None);
        let [near, far] = h.partition(left).unwrap();
        let incomplete = h.united(Some(near), Some(right)).unwrap();
        assert_eq!(h.sources(&incomplete), None);
        let restored = h.united(Some(incomplete), Some(far)).unwrap();
        assert_eq!(
            h.sources(&restored),
            Some(vec![
                BoundarySource {
                    input: 0,
                    whole: true
                },
                BoundarySource {
                    input: 1,
                    whole: true
                }
            ])
        );
        // A real complete parent also dominates a duplicated nested partial.
        let [piece, _] = h.partition(parent.clone()).unwrap();
        let [nested, _] = h.partition(piece).unwrap();
        let with_copy = h.united(Some(parent), Some(nested)).unwrap();
        assert_eq!(
            h.sources(&with_copy),
            Some(vec![
                BoundarySource {
                    input: 0,
                    whole: true
                },
                BoundarySource {
                    input: 1,
                    whole: true
                }
            ])
        );
    }

    #[test]
    fn unproved_ancestry_is_not_a_certified_singleton_subpiece() {
        let mut h = BoundaryHistory::default();
        let a = h.original(3).unwrap();
        let uncertain = h.uncertain(Some(a.clone())).unwrap();
        assert_eq!(h.sources(&uncertain), None);
        let [left, right] = h.partition(uncertain).unwrap();
        assert_eq!(h.sources(&left), None);
        let reunited_unknown = h.united(Some(left), Some(right)).unwrap();
        assert_eq!(h.sources(&reunited_unknown), None);
        // Matching a root never proves the generic event's current finite domain.
        let root_also_present = h.united(Some(a), Some(reunited_unknown)).unwrap();
        assert_eq!(h.sources(&root_also_present), None);
    }

    #[test]
    fn certificate_indices_from_other_arenas_are_never_reinterpreted() {
        let mut first = BoundaryHistory::default();
        let alien = first.original(11).unwrap();
        let mut second = BoundaryHistory::default();
        let local = second.original(22).unwrap();
        assert_eq!(alien.index, local.index);
        assert_eq!(second.sources(&alien), None);
        assert_eq!(second.copy_atoms(&alien), None);
        assert_eq!(second.covered_by(&[Atom::Root(22)], &alien), None);
        assert_eq!(second.roots_of(alien.clone()), None);
        assert_eq!(second.united(Some(alien), Some(local)), None);
    }

    #[test]
    fn exhausted_or_disabled_history_never_reseeds_certificates() {
        let mut h = BoundaryHistory::default();
        let root = h.original(8).unwrap();
        h.proof_work().unwrap().disable();
        assert_eq!(h.sources(&root), None);
        assert_eq!(h.original(8), None);
        assert_eq!(h.original(9), None);
        let mut disabled = BoundaryHistory::untracked();
        assert_eq!(disabled.original(0), None);
        assert!(disabled.owner.is_none());
    }
    fn rectangle(
        k: &mut Kernel,
        history: &mut BoundaryHistory,
        lower: [f64; 2],
        upper: [f64; 2],
        base: usize,
    ) -> (Brep, EdgeSources) {
        use crate::curve::{curve::Curve, line::Line};
        use crate::geom::vector3d::Point3;
        let points = [
            Point3::new(lower[0], lower[1], 0.0),
            Point3::new(upper[0], lower[1], 0.0),
            Point3::new(upper[0], upper[1], 0.0),
            Point3::new(lower[0], upper[1], 0.0),
        ];
        let curves: Vec<Curve> = points
            .iter()
            .zip(points.iter().cycle().skip(1))
            .take(4)
            .map(|(a, b)| Line::create_line_segment(3, *a, *b, None).unwrap().into())
            .collect();
        let (body, made) =
            crate::primitive::api_trim_ops::create_planar_faces_with_edges(k, &curves).unwrap();
        let sources = made
            .into_iter()
            .map(|(edge, original)| (edge, history.original(base + original)))
            .collect();
        (body, sources)
    }

    #[test]
    fn exhausted_history_keeps_the_actual_planar_boolean_geometry() {
        let fixture = |exhaust: Option<bool>| {
            let mut k = Kernel::default();
            let mut history = BoundaryHistory::default();
            let (a, first) = rectangle(&mut k, &mut history, [0.0, 0.0], [10.0, 10.0], 0);
            let (b, second) = rectangle(&mut k, &mut history, [5.0, -5.0], [15.0, 5.0], 4);
            if exhaust == Some(true) {
                history.proof_work().unwrap().disable();
            }
            assert!(k.context.proof_work().is_none());
            let (outcome, sources) = if exhaust.is_some() {
                run(
                    &mut k,
                    a,
                    b,
                    Boolean2d::Intersection,
                    &first,
                    &second,
                    &mut history,
                )
                .unwrap()
            } else {
                (
                    super::super::api_brep::boolean_2d(&mut k, a, b, Boolean2d::Intersection)
                        .unwrap(),
                    EdgeSources::new(),
                )
            };
            assert!(k.context.proof_work().is_none());
            let Boolean2dOutcome::Result(body) = outcome else {
                panic!("the intersection must exist")
            };
            let mut points: Vec<_> = crate::topology::brep::p1::vertices(&body, None)
                .unwrap()
                .into_iter()
                .map(|v| {
                    let p = body.vertex(v).unwrap().point;
                    [p.x, p.y, p.z]
                })
                .collect();
            points.sort_by(|a, b| {
                a[0].total_cmp(&b[0])
                    .then(a[1].total_cmp(&b[1]))
                    .then(a[2].total_cmp(&b[2]))
            });
            assert_eq!(
                points,
                vec![
                    [5.0, 0.0, 0.0],
                    [5.0, 5.0, 0.0],
                    [10.0, 0.0, 0.0],
                    [10.0, 5.0, 0.0]
                ]
            );
            let known = sources
                .values()
                .filter_map(Clone::clone)
                .any(|id| history.sources(&id).is_some());
            if exhaust == Some(false) {
                assert!(
                    known,
                    "the normal producer path must actually preserve a certificate"
                );
            } else {
                assert!(!known);
            }
            points
                .into_iter()
                .map(|p| p.map(f64::to_bits))
                .collect::<Vec<_>>()
        };
        let ordinary = fixture(None);
        assert_eq!(
            ordinary,
            fixture(Some(false)),
            "an active observer must preserve the unobserved geometry bits"
        );
        assert_eq!(
            ordinary,
            fixture(Some(true)),
            "optional observer exhaustion must preserve the unobserved geometry bits"
        );
    }

    fn actual_bottom_sources(
        body: &Brep,
        sources: &EdgeSources,
        history: &mut BoundaryHistory,
        right: f64,
    ) -> Vec<BoundarySource> {
        let mut bottoms = Vec::new();
        for (&edge, certificate) in sources {
            let (a, b) = edge::start_end_vertices(body, edge).unwrap();
            let mut ends = [a, b].map(|vertex| {
                let point = body.vertex(vertex.unwrap()).unwrap().point;
                [point.x, point.y, point.z]
            });
            ends.sort_by(|a, b| a[0].total_cmp(&b[0]));
            if ends == [[0.0, 0.0, 0.0], [right, 0.0, 0.0]] {
                bottoms.push(certificate);
            }
        }
        assert_eq!(
            bottoms.len(),
            1,
            "select the one actual complete bottom edge"
        );
        history.sources(bottoms[0].as_ref().unwrap()).unwrap()
    }

    #[test]
    fn actual_boolean_rounds_rejoin_every_original_boundary_partition() {
        let mut k = Kernel::default();
        let mut history = BoundaryHistory::default();
        let (a, sa) = rectangle(&mut k, &mut history, [0.0, 0.0], [10.0, 10.0], 0);
        let (b, sb) = rectangle(&mut k, &mut history, [5.0, 0.0], [15.0, 10.0], 4);
        let (c, sc) = rectangle(&mut k, &mut history, [12.0, 0.0], [20.0, 10.0], 8);
        let (first, sources) = run(&mut k, a, b, Boolean2d::Union, &sa, &sb, &mut history).unwrap();
        let Boolean2dOutcome::Result(first) = first else {
            panic!("the first union must exist")
        };
        let complete_bottom = |values: &[BoundarySource], inputs: &[usize]| {
            inputs.iter().all(|input| {
                values.contains(&BoundarySource {
                    input: *input,
                    whole: true,
                })
            })
        };
        assert!(
            complete_bottom(
                &actual_bottom_sources(&first, &sources, &mut history, 15.0),
                &[0, 4]
            ),
            "the first actual split/alias/rejoin must retain both whole bottom parents"
        );
        // Rebind these actual current edges in a fresh Collector; never reseed
        // the already mixed intermediate edge as a new whole original root.
        let (second, sources) = run(
            &mut k,
            first,
            c,
            Boolean2d::Union,
            &sources,
            &sc,
            &mut history,
        )
        .unwrap();
        let Boolean2dOutcome::Result(second) = second else {
            panic!("the second union must exist")
        };
        assert!(
            complete_bottom(
                &actual_bottom_sources(&second, &sources, &mut history, 20.0),
                &[0, 4, 8]
            ),
            "nested partitions of the mixed intermediate must rejoin across all actual rounds"
        );
        let points: Vec<_> = crate::topology::brep::p1::vertices(&second, None)
            .unwrap()
            .into_iter()
            .map(|v| {
                let p = second.vertex(v).unwrap().point;
                [p.x, p.y]
            })
            .collect();
        for expected in [[0.0, 0.0], [20.0, 0.0], [20.0, 10.0], [0.0, 10.0]] {
            assert!(points.contains(&expected));
        }
        assert_eq!(points.len(), 4);
    }

    #[test]
    fn generic_edge_alias_notifications_do_not_certify_disjoint_finite_domains() {
        let mut k = Kernel::default();
        let mut history = BoundaryHistory::default();
        let (a, sa) = rectangle(&mut k, &mut history, [0.0, 0.0], [1.0, 1.0], 0);
        let (b, sb) = rectangle(&mut k, &mut history, [3.0, 0.0], [4.0, 1.0], 4);
        let first = *crate::topology::brep::p1::edges(&a, None)
            .unwrap()
            .first()
            .unwrap();
        let second = *crate::topology::brep::p1::edges(&b, None)
            .unwrap()
            .first()
            .unwrap();
        let ar = notify_ref(EntityRef::Edge(first));
        let br = copy_source_ref(EntityRef::Edge(second));
        let mut collector = Collector {
            sources: BTreeMap::from([
                (ar.key, sa.get(&first).cloned().flatten()),
                (br.key, sb.get(&second).cloned().flatten()),
            ]),
            history,
            deferred: BTreeMap::new(),
            births: BTreeMap::new(),
            pending: Vec::new(),
            incomplete: false,
        };
        // These notification contracts do not carry a finite-domain predicate.
        // Receiver safety cannot assume a real geometric alias from the names.
        collector
            .execute(N::Coincident, ar, Some(ar), Some(br), None)
            .unwrap();
        let value = collector.value(ar.key).unwrap();
        assert_eq!(collector.history.sources(&value), None);
        collector
            .execute(N::Copy, br, Some(ar), None, None)
            .unwrap();
        let value = collector.value(ar.key).unwrap();
        assert_eq!(collector.history.sources(&value), None);
    }
    #[test]
    fn proof_queries_and_later_recipe_rounds_share_the_certificate_allowance() {
        let mut history = BoundaryHistory::default();
        let root = history.original(13).unwrap();
        let query = history.proof_work().unwrap();
        assert!(query.same_operation(&root.owner));
        assert!(!query.charge(Some(u64::MAX)));
        assert_eq!(history.sources(&root), None);
        assert_eq!(history.original(13), None);
        assert!(BoundaryHistory::untracked().proof_work().is_none());
    }

    #[test]
    fn actual_planar_iir_split_notifications_reach_the_custom_observer_for_both_operands() {
        use std::sync::{Arc, Mutex};

        #[derive(Debug)]
        struct Splits {
            work: ProofWork,
            parents: Arc<Mutex<Vec<ObjectKey>>>,
        }
        impl NotifyCallback for Splits {
            fn execute(
                &mut self,
                _operation: N,
                _caller: ObjectRef,
                _a: Option<ObjectRef>,
                _b: Option<ObjectRef>,
                _c: Option<ObjectRef>,
            ) -> Result<()> {
                Ok(())
            }
            fn reserve_boundary_edit(&mut self, work: usize) -> bool {
                self.work.charge(u64::try_from(work).ok())
            }
            fn boundary_edit(&mut self, event: BoundaryEdit) {
                if let BoundaryEdit::BeginSplit { parent, .. } = event {
                    self.parents.lock().unwrap().push(parent.key);
                }
            }
        }

        // The intersection cuts two edges of each rectangle. Later cleanup only
        // joins/removes edges; the actual split constructors are in planar IIR.
        let mut k = Kernel::default();
        let mut history = BoundaryHistory::default();
        let (a, _) = rectangle(&mut k, &mut history, [0.0, 0.0], [10.0, 10.0], 0);
        let (b, _) = rectangle(&mut k, &mut history, [5.0, -5.0], [15.0, 5.0], 4);
        let first_keys: Vec<_> = a
            .edge_list()
            .iter()
            .map(|e| notify_ref(EntityRef::Edge(*e)).key)
            .collect();
        let second_keys: Vec<_> = b
            .edge_list()
            .iter()
            .map(|e| copy_source_ref(EntityRef::Edge(*e)).key)
            .collect();
        let work = ProofWork::new(crate::prism::sheet::READ_WORK);
        let parents = Arc::new(Mutex::new(Vec::new()));
        let mut context = Context::default();
        context.set_proof_work(Some(work.clone()));
        context.set_user_notify_callback(Some(Box::new(Splits {
            work: work.clone(),
            parents: parents.clone(),
        })));
        let outcome =
            boolean_2d_observed(&mut k, a, b, Boolean2d::Intersection, Some(&mut context)).unwrap();
        let Boolean2dOutcome::Result(body) = outcome else {
            panic!("overlapping rectangles must intersect");
        };
        assert_eq!(body.edge_list().len(), 4);
        let parents = parents.lock().unwrap();
        assert!(
            parents.iter().any(|parent| first_keys.contains(parent)),
            "primary IIR split reaches custom observer"
        );
        assert!(
            parents.iter().any(|parent| second_keys.contains(parent)),
            "other IIR split uses its distinct caller keys"
        );
        assert!(!work.is_disabled());
        assert!(k.context.user_notify_callback().is_none());
        assert!(k.context.sys_notify_callback().is_none());
    }

    fn alias_fixture() -> (Collector, [ObjectRef; 4]) {
        // Pure protocol identities in one synthetic notification domain, not a
        // geometric alias assertion; actual geometry proof is a separate gate.
        let refs = [11, 22, 33, 44].map(|key| ObjectRef {
            type_code: edge::TYPE,
            key: ObjectKey(key),
        });
        let mut history = BoundaryHistory::default();
        let second = history.original(7).unwrap();
        let third = history.original(8).unwrap();
        let collector = Collector {
            sources: BTreeMap::from([(refs[1].key, Some(second)), (refs[2].key, Some(third))]),
            history,
            deferred: BTreeMap::new(),
            births: BTreeMap::new(),
            pending: Vec::new(),
            incomplete: false,
        };
        (collector, refs)
    }

    fn construct_dependency(c: &mut Collector, target: ObjectRef, opposite: ObjectRef) {
        c.edit(BoundaryEdit::FreshAllocated { target });
        c.execute(
            N::ChangeGeometry,
            target,
            Some(ObjectRef {
                type_code: -1,
                key: ObjectKey(99),
            }),
            None,
            None,
        )
        .unwrap();
        c.edit(BoundaryEdit::FreshDependency { target, opposite });
    }

    fn alias_copies(c: &mut Collector, first: ObjectRef, second: ObjectRef) {
        c.execute(N::Copy, second, Some(first), None, None).unwrap();
        c.execute(N::Copy, first, Some(second), None, None).unwrap();
    }

    fn projected(c: &mut Collector, r: ObjectRef) -> Option<Vec<BoundarySource>> {
        c.value(r.key).and_then(|id| c.history.sources(&id))
    }

    #[test]
    fn delivered_alias_bracket_resolves_only_the_exact_clean_fresh_dependency() {
        let (mut c, [a, b, _, _]) = alias_fixture();
        construct_dependency(&mut c, a, b);
        assert!(c.value(a.key).is_none());
        c.edit(BoundaryEdit::BeginAlias {
            first: a,
            second: b,
        });
        alias_copies(&mut c, a, b);
        assert_eq!(
            projected(&mut c, a),
            None,
            "generic copies alone remain unproved"
        );
        c.edit(BoundaryEdit::CommitAlias {
            first: a,
            second: b,
        });
        let expected = Some(vec![BoundarySource {
            input: 7,
            whole: true,
        }]);
        assert_eq!(projected(&mut c, a), expected);
        assert_eq!(projected(&mut c, b), expected);
        assert!(!c.incomplete);

        let (mut c, [a, b, other, _]) = alias_fixture();
        construct_dependency(&mut c, a, other);
        c.edit(BoundaryEdit::BeginAlias {
            first: a,
            second: b,
        });
        alias_copies(&mut c, a, b);
        c.edit(BoundaryEdit::CommitAlias {
            first: a,
            second: b,
        });
        assert_eq!(
            projected(&mut c, a),
            None,
            "a different current pair cannot resolve the dependency"
        );
    }

    #[test]
    fn mutually_deferred_current_pair_cannot_seed_a_certificate() {
        let (mut c, [a, b, _, _]) = alias_fixture();
        c.sources.remove(&b.key);
        construct_dependency(&mut c, a, b);
        construct_dependency(&mut c, b, a);
        c.edit(BoundaryEdit::BeginAlias {
            first: a,
            second: b,
        });
        alias_copies(&mut c, a, b);
        c.edit(BoundaryEdit::CommitAlias {
            first: a,
            second: b,
        });
        assert_eq!(projected(&mut c, a), None);
        assert_eq!(projected(&mut c, b), None);
        assert!(!c.incomplete);
    }

    #[test]
    fn alias_proof_does_not_heal_prior_unknown_or_reused_constructor_targets() {
        for unknown in [false, true] {
            let (mut c, [a, b, other, _]) = alias_fixture();
            if unknown {
                c.sources.insert(a.key, None);
            } else {
                c.execute(N::Copy, other, Some(a), None, None).unwrap();
            }
            construct_dependency(&mut c, a, b);
            assert!(!c.deferred.contains_key(&a.key));
            c.edit(BoundaryEdit::BeginAlias {
                first: a,
                second: b,
            });
            alias_copies(&mut c, a, b);
            c.edit(BoundaryEdit::CommitAlias {
                first: a,
                second: b,
            });
            assert_eq!(projected(&mut c, a), None);
            assert_eq!(projected(&mut c, b), None);
        }
    }

    #[test]
    fn extra_copy_or_unknown_inside_alias_bracket_never_restores_known_sources() {
        for unknown in [false, true] {
            let (mut c, [a, b, other, _]) = alias_fixture();
            construct_dependency(&mut c, a, b);
            c.edit(BoundaryEdit::BeginAlias {
                first: a,
                second: b,
            });
            c.execute(N::Copy, b, Some(a), None, None).unwrap();
            if unknown {
                c.execute(N::Unknown, other, None, None, None).unwrap();
            } else {
                c.execute(N::Copy, other, Some(a), None, None).unwrap();
            }
            c.execute(N::Copy, a, Some(b), None, None).unwrap();
            c.edit(BoundaryEdit::CommitAlias {
                first: a,
                second: b,
            });
            assert_eq!(projected(&mut c, a), None);
        }
    }

    #[test]
    fn incomplete_copy_pair_has_no_alias_commit_certificate() {
        for copies in 0..2 {
            let (mut c, [a, b, _, _]) = alias_fixture();
            construct_dependency(&mut c, a, b);
            c.edit(BoundaryEdit::BeginAlias {
                first: a,
                second: b,
            });
            if copies == 1 {
                c.execute(N::Copy, b, Some(a), None, None).unwrap();
            }
            // A failed original notify returns before CommitAlias is emitted.
            assert!(c
                .pending
                .iter()
                .any(|p| matches!(p, Some(Pending::Alias { .. }))));
            assert_eq!(projected(&mut c, a), None);
        }
    }

    #[test]
    fn stale_or_unfinished_fresh_dependencies_remain_unknown_per_edge() {
        for event in [N::PreEdit, N::ChangeGeometry, N::Destruction] {
            let (mut c, [a, b, _, _]) = alias_fixture();
            construct_dependency(&mut c, a, b);
            c.execute(event, b, None, None, None).unwrap();
            assert!(!c.deferred.contains_key(&a.key));
            c.edit(BoundaryEdit::BeginAlias {
                first: a,
                second: b,
            });
            alias_copies(&mut c, a, b);
            c.edit(BoundaryEdit::CommitAlias {
                first: a,
                second: b,
            });
            assert_eq!(projected(&mut c, a), None);
        }
        let (mut c, [a, b, _, _]) = alias_fixture();
        c.edit(BoundaryEdit::FreshAllocated { target: a });
        assert_eq!(projected(&mut c, a), None);
        assert!(projected(&mut c, b).is_some());
        assert!(
            !c.incomplete,
            "unsupported birth does not discard unrelated known edges"
        );
        // Two installations are not one clean constructor initialization.
        c.execute(N::ChangeGeometry, a, Some(b), None, None)
            .unwrap();
        c.execute(N::ChangeGeometry, a, Some(b), None, None)
            .unwrap();
        c.edit(BoundaryEdit::FreshDependency {
            target: a,
            opposite: b,
        });
        assert!(!c.deferred.contains_key(&a.key));
    }

    #[test]
    fn queued_alias_begin_snapshots_after_prior_delivered_producer_events() {
        use crate::geom::object::{notify, report_boundary_edit};
        let (mut c, [a, b, other, _]) = alias_fixture();
        let old = c.history.original(1).unwrap();
        c.sources.insert(a.key, Some(old));
        let mut k = Kernel::default();
        k.context.set_proof_work(c.history.proof_work());
        k.context.set_user_notify_callback(Some(Box::new(c)));
        Context::with_committed_notifications(&mut k, |k| {
            // This must replace the old snapshot BEFORE BeginAlias is delivered.
            report_boundary_edit(
                Some(&mut k.context),
                BoundaryEdit::Copied {
                    source: other,
                    target: a,
                },
            );
            report_boundary_edit(
                Some(&mut k.context),
                BoundaryEdit::BeginAlias {
                    first: a,
                    second: b,
                },
            );
            notify(Some(&mut k.context), N::Copy, b, Some(a), None, None);
            notify(Some(&mut k.context), N::Copy, a, Some(b), None, None);
            report_boundary_edit(
                Some(&mut k.context),
                BoundaryEdit::CommitAlias {
                    first: a,
                    second: b,
                },
            );
            Ok(())
        })
        .unwrap();
        let c = k
            .context
            .user_notify_callback_mut()
            .unwrap()
            .as_any_mut()
            .unwrap()
            .downcast_mut::<Collector>()
            .unwrap();
        let expected = Some(vec![
            BoundarySource {
                input: 7,
                whole: true,
            },
            BoundarySource {
                input: 8,
                whole: true,
            },
        ]);
        assert_eq!(projected(c, a), expected);
        assert_eq!(projected(c, b), expected);
        assert!(!c.incomplete);
        assert!(!c.pending.iter().any(Option::is_some));
    }

    #[test]
    fn alias_copy_with_extra_payload_is_not_a_bracket_owned_copy() {
        for third_payload in [false, true] {
            let (mut c, [a, b, contributor, _]) = alias_fixture();
            construct_dependency(&mut c, a, b);
            c.edit(BoundaryEdit::BeginAlias {
                first: a,
                second: b,
            });
            // The same source/target does not authorize an extra contribution.
            let (second, third) = if third_payload {
                (None, Some(contributor))
            } else {
                (Some(contributor), None)
            };
            c.execute(N::Copy, b, Some(a), second, third).unwrap();
            c.execute(N::Copy, a, Some(b), None, None).unwrap();
            c.edit(BoundaryEdit::CommitAlias {
                first: a,
                second: b,
            });
            assert_eq!(projected(&mut c, a), None);
            assert_eq!(projected(&mut c, b), None);
        }
    }

    #[test]
    fn alias_pending_storage_fits_the_canonical_growth_credit() {
        // The work allowance is target-independent. This catches a future
        // representation growth; it never changes quota based on host sizeof.
        assert!(size_of::<Option<Pending>>() <= 192);
    }

    #[test]
    fn an_uncertified_interval_edit_discards_an_existing_whole_certificate() {
        let (mut c, [fresh, edited, _, _]) = alias_fixture();
        assert_eq!(
            projected(&mut c, edited),
            Some(vec![BoundarySource {
                input: 7,
                whole: true,
            }])
        );
        // PreEdit itself precedes both proved splits and unsupported interval
        // edits. Only the actual producer knows which subsequently occurred.
        c.execute(N::PreEdit, edited, Some(edited), None, None)
            .unwrap();
        c.edit(BoundaryEdit::UncertifiedExtentChanged { edge: edited });
        assert_eq!(projected(&mut c, edited), None);
        construct_dependency(&mut c, fresh, edited);
        c.edit(BoundaryEdit::BeginAlias {
            first: fresh,
            second: edited,
        });
        alias_copies(&mut c, fresh, edited);
        c.edit(BoundaryEdit::CommitAlias {
            first: fresh,
            second: edited,
        });
        assert_eq!(projected(&mut c, fresh), None);
        assert_eq!(projected(&mut c, edited), None);
    }

    #[test]
    fn an_uncertified_interval_edit_cannot_be_hidden_by_a_pending_partition() {
        let (mut c, [upper, parent, _, _]) = alias_fixture();
        c.edit(BoundaryEdit::BeginSplit {
            parent,
            lower: parent,
            upper,
            bounds: [0.0, 1.0],
            at: 0.5,
        });
        c.edit(BoundaryEdit::UncertifiedExtentChanged { edge: parent });
        assert!(
            c.incomplete,
            "a pending snapshot cannot repair an unrelated domain edit"
        );
    }

    #[test]
    fn completed_alias_brackets_do_not_accumulate_empty_slots() {
        let (mut collector, [first, second, ..]) = alias_fixture();
        construct_dependency(&mut collector, first, second);
        for _ in 0..32 {
            collector.begin_alias(first, second);
            assert_eq!(collector.pending.len(), 1);
            alias_copies(&mut collector, first, second);
            collector.commit_alias(first, second);
            assert!(collector.pending.is_empty());
            assert!(!collector.incomplete);
            assert!(!collector.history.is_exhausted());
            assert_eq!(
                projected(&mut collector, first),
                Some(vec![BoundarySource {
                    input: 7,
                    whole: true
                },])
            );
        }
    }

    #[test]
    fn pending_compaction_keeps_outer_brackets_and_declines_only_metadata() {
        let (mut collector, [first, second, third, fourth]) = alias_fixture();
        collector.begin_join(first, second);
        collector.begin_join(third, fourth);
        collector.commit_join(third, fourth);
        assert_eq!(collector.pending.len(), 1);
        assert!(matches!(collector.pending[0],
            Some(Pending::Join { survivor, removed, .. })
                if survivor == first.key && removed == second.key));
        collector.commit_join(first, second);
        assert!(collector.pending.is_empty());
        collector.begin_join(first, second);
        collector.history.proof_work().unwrap().disable();
        collector.commit_join(first, second);
        assert!(collector.history.is_exhausted());
    }
}
