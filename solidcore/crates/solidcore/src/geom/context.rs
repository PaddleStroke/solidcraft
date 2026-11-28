//! Per-session modeling context (`SmContext`): traversal marks and their locks, the
//! "doing a Boolean" flag, notify callbacks and the context's own model size estimate
//! and zone tolerance; plus the mark-locking helper `SmNewMarkAndLock`, the
//! `SmMassPropertyBehavior` options and the `SM_API` context accessors.
//!
//! Upstream: `source/SMLib/inc/SmContext.h`, `source/SMLib/src/SmContext.cpp`,
//! `source/SMLib/inc/SmTopoTypes.h` (`SmMassPropertyBehavior`),
//! `source/SM_API/src/SmApiGeneral.cpp` (`SmApiCreateContext`, `SmApiGetOrCreateContext`).
//!
//! | Rust                                      | Upstream                                        |
//! |-------------------------------------------|-------------------------------------------------|
//! | [`Context`]                               | `SmContext`                                     |
//! | [`Context::new`], [`Context::default`]    | `SmContext::SmContext(double, double)` (no `SM_USE_GLOBAL_CACHE`) |
//! | [`Context::doing_boolean`]                | `SmContext::GetDoingBoolean`                    |
//! | [`IdentitySlot`], [`NotifyLease`], [`Context::take_notify_lease`], [`Context::restore_notify_lease`], [`KERNEL_KEY_BASE`], [`TOPOLOGY_KEY_BASE`], [`GEOMETRY_KEY_BASE`], [`MAX_BREP_SERIAL`], [`is_reserved_key`] | (none: kernel-issued notification identities; CO-PORTED by `topology.brep_p3`) |
//! | `Context::share_geometry_sequence` (crate) | store-owned allocation of geometry notification keys |
//! | [`Context::doing_boolean_mut`]            | `SmContext::GetDoingBooleanRef`                 |
//! | [`Context::set_doing_boolean`]            | `SmContext::SetDoingBoolean`                    |
//! | [`Context::user_notify_callback`]         | `SmContext::GetUserNotifyCallback`              |
//! | [`Context::set_user_notify_callback`]     | `SmContext::SetUserNotifyCallback` (unused upstream) |
//! | [`Context::sys_notify_callback`]          | `SmContext::GetSysNotifyCallback`               |
//! | [`Context::user_notify_callback_mut`], [`Context::sys_notify_callback_mut`] | the same getters, for `Execute` |
//! | [`NotifyCallback`], [`NotifyCallback::execute`] | `SmNotifyCallback`, `SmNotifyCallback::Execute` |
//! | [`NotifyCallback::owner_link`]            | (none: the `GetOwner()` / `GetEdge()` a callback reads) |
//! | [`Context::set_sys_notify_callback`]      | `SmContext::SetSysNotifyCallback`               |
//! | [`Context::this_model_size_estimate`]     | `SmContext::GetThisModelSizeEstimate`           |
//! | [`Context::set_this_model_size_estimate`] | `SmContext::SetThisModelSizeEstimate` (inline, `SM_USE_OLDTOL` build) |
//! | [`Context::set_this_large_small_size_ratio`] | `SmContext::SetThisLargeSmallSizeRatio`      |
//! | [`Context::this_zone_tol_3d`]             | `SmContext::m_sThisZoneTol3d` (`GetThisZoneTol3d`, unused upstream) |
//! | [`Context::stored_model_size_estimate`], [`Context::stored_large_small_size_ratio`] | the raw members `m_dThisModelSizeEstimate`, `m_dThisLargeSmallSizeRatio` |
//! | [`Context::current_mark`]                 | `SmContext::GetCurrentMark`                     |
//! | [`Context::is_mark_locked`]               | `SmContext::IsMarkLocked`                       |
//! | `Context::new_mark` (private)             | `SmContext::NewMark` (private, friend `SmNewMarkAndLock`) |
//! | [`MarkType`]                              | `SmMarkType`                                    |
//! | [`MarkLock`]                              | `SmNewMarkAndLock`                              |
//! | [`MarkLock::new`]                         | `SmNewMarkAndLock(const SmContext*, SmBitArray)` |
//! | [`MarkLock::new_pair`] | `SmNewMarkAndLock(SmContext*, SmContext*)` |
//! | [`MarkLock::unbound`], [`MarkLock::default`] | `SmNewMarkAndLock()`                         |
//! | [`MarkLock::release`]                     | `SmNewMarkAndLock::~SmNewMarkAndLock`           |
//! | [`MarkLock::mark_type`], [`MarkLock::mark_type1`], [`MarkLock::mark_type2`] | `GetMarkType`, `GetMarkType1`, `GetMarkType2` |
//! | [`MarkLock::context`]                     | `SmNewMarkAndLock::GetContext`                  |
//! | [`MarkLock::context2`]                    | `SmNewMarkAndLock::GetContext2`                 |
//! | [`MarkLock::new_mark`]                    | `SmNewMarkAndLock::NewMark`                     |
//! | [`MarkLock::set_context`]                 | `SmNewMarkAndLock::SetContext(SmContext*, SmMarkType)` |
//! | [`MarkLock::set_context_pair`]            | `SmNewMarkAndLock::SetContext(SmContext*, SmContext*)` |
//! | `lock_mark`, `unlock_mark` (private)      | `SmNewMarkAndLock::LockMark`, `UnlockMark`      |
//! | `unlocked_mark` (private)                 | `SmNewMarkAndLock::GetUnlockedMark`             |
//! | [`with_mark_lock`]                        | the scoped `{ SmNewMarkAndLock sMarkLock(pContext, mask); ... }` idiom |
//! | [`Context::claim_mark`], [`SharedMarkLock`] | the same idiom in a `const` query (`SmNewMarkAndLock(const SmContext*, mask)`) |
//! | [`MassPropertyBehavior`]                  | `SmMassPropertyBehavior` (and its constructor)  |
//! | [`create_context`]                        | `SmApiCreateContext`                            |
//! | [`get_or_create_context`]                 | `SmApiGetOrCreateContext`                       |
//!
//! Deviations:
//! - `SmNewMarkAndLock` holds `SmContext*` pointers and unlocks in its destructor.
//!   [`MarkLock`] instead stores the *indices* of its contexts in a caller-owned slice
//!   `&mut [Context]` (index equality plays pointer equality) and every method that
//!   touches a context takes that slice; the destructor is the explicit
//!   [`MarkLock::release`]. [`with_mark_lock`] gives the scoped form for a
//!   [`Kernel`]'s own context. A method that upstream would run through a null
//!   context pointer (a crash) returns [`SolidError::InvalidInput`], as does an index
//!   outside the slice.
//! - The marks and their locks are atomics, so that a `const` query that only holds the
//!   context shared (the port's `&Kernel`) can still claim a mark as upstream's do
//!   ([`Context::claim_mark`], whose guard unlocks on drop). The kernel is
//!   single-threaded: the accesses happen in program order.
//! - In `SmNewMarkAndLock(SmContext*, SmContext*)`, when `ctx1 != ctx2`, pinned v1.0.1
//!   picks `mark_type2` from the second context but increments the first context's mark
//!   twice and locks `mark_type1` in the second context, while the destructor unlocks
//!   `mark_type2` there (`WR-1`, `plan/findings/geom.context--notes.md`).
//!   [`MarkLock::new_pair`] advances and locks `(ctx2, mark_type2)`, so releasing every
//!   lock leaves no mark locked.
//! - `SM_ASSERT*`, `SM_ASSERT_MSG_BREAK` and `SM_DBG_WARN` only print messages in the
//!   release build and are not ported. `SmBoolean` flags are `bool`; `ULONG` marks are
//!   `u64` incremented with wrapping arithmetic, as in C.
//! - Notify callbacks are boxed [`NotifyCallback`] trait objects owned by the context
//!   (upstream stores borrowed raw pointers); `Execute` takes `&mut self` and is
//!   reached through [`Context::user_notify_callback_mut`] /
//!   [`Context::sys_notify_callback_mut`] by [`object::notify`].
//! - `SmApiGeneral.cpp` keeps one process-global `SmContext*`; the port takes the slot
//!   (`&mut Option<Context>`) from the caller.
//! - Store-owned contexts can share a geometry-key sequence without sharing their
//!   identity domains. The counter is owned, bounded and has no process-global state.
//! - The attribute-callback list, global user data (`void*`), `operator new/delete`,
//!   `Dump`, `GetTypeString` and `IsLockedMark` are unused upstream and not ported.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex,
};

use crate::error::{Result, SolidError};
use crate::geom::attribute::ObjectKey;
use crate::geom::object;
use crate::geom::tol;
use crate::kernel::Kernel;

/// One operation-owned optional metadata allowance. All query copies share it.
/// Canonical units are architecture-independent; exhaustion is permanently unknown.
#[derive(Clone, Debug)]
pub(crate) struct ProofWork {
    state: Arc<ProofWorkState>,
}

#[derive(Debug)]
struct ProofWorkState {
    remaining: AtomicUsize,
    disabled: AtomicBool,
    limit: usize,
}

impl ProofWork {
    /// Creates the existing observer allowance, including its fixed owner storage.
    pub(crate) fn new(limit: usize) -> Self {
        const OWNER_WORK: usize = 64;
        Self {
            state: Arc::new(ProofWorkState {
                remaining: AtomicUsize::new(limit.saturating_sub(OWNER_WORK)),
                disabled: AtomicBool::new(limit < OWNER_WORK),
                limit,
            }),
        }
    }

    /// Debits checked canonical work before optional computation or allocation.
    pub(crate) fn charge(&self, amount: Option<u64>) -> bool {
        let Some(amount) = amount.and_then(|n| usize::try_from(n).ok()) else {
            self.disable();
            return false;
        };
        if self.is_disabled() {
            return false;
        }
        // As in geometry_key: preserve MSRV 1.85 while 1.99 renames this API.
        #[allow(deprecated, reason = "try_update is newer than 1.85 (#14360)")]
        let charged =
            self.state
                .remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    left.checked_sub(amount)
                });
        if charged.is_ok() {
            return !self.is_disabled();
        }
        self.disable();
        false
    }

    /// Stops only the optional proof; it never changes a geometry operation.
    pub(crate) fn disable(&self) {
        self.state.disabled.store(true, Ordering::Relaxed);
        self.state.remaining.store(0, Ordering::Relaxed);
    }

    /// Whether a prior decline permanently discarded this operation's evidence.
    pub(crate) fn is_disabled(&self) -> bool {
        self.state.disabled.load(Ordering::Relaxed)
    }

    /// Retains the existing opaque certificate-owner equality.
    pub(crate) fn same_operation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// Retains the existing cross-owner certificate ordering, never a source ID.
    pub(crate) fn operation_key(&self) -> usize {
        Arc::as_ptr(&self.state) as usize
    }

    /// The real optional allowance is spent; caught only at its query boundary.
    pub(crate) fn stopped(&self, operation: &'static str) -> SolidError {
        let remaining = self.state.remaining.load(Ordering::Relaxed);
        SolidError::OutOfFuel {
            operation,
            spent: self.state.limit.saturating_sub(remaining) as u64,
            limit: self.state.limit as u64,
        }
    }
}

/// Which traversal mark of a context to use (`SmMarkType`). The discriminants are the
/// upstream bit values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(i32)]
pub enum MarkType {
    /// `SM_MT_NOMARK`: no mark (also the unset value of a [`MarkLock`]).
    NoMark = 0,
    /// `SM_MT_MARK`: the general mark.
    Mark = 1,
    /// `SM_MT_MARK2`: the second general mark.
    Mark2 = 2,
    /// `SM_MT_MARK3`: the third general mark.
    Mark3 = 4,
    /// `SM_MT_MARKIO`: the mark used by draw / IO methods.
    MarkIo = 8,
    /// `SM_MT_MARKASSERT`: the mark used by validity checks.
    MarkAssert = 16,
    /// `SM_MT_ALLMARKS`: all marks; as a single mark it behaves like an unknown
    /// value (the `default:` branches upstream).
    AllMarks = 31,
}

impl MarkType {
    /// The upstream enum value (also the bit of the mark in a mark mask).
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }

    /// The mark type with upstream value `code`, if any.
    #[must_use]
    pub const fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(Self::NoMark),
            1 => Some(Self::Mark),
            2 => Some(Self::Mark2),
            4 => Some(Self::Mark3),
            8 => Some(Self::MarkIo),
            16 => Some(Self::MarkAssert),
            31 => Some(Self::AllMarks),
            _ => None,
        }
    }

    /// Index of the mark's counter and lock in a [`Context`]; `None` for
    /// [`MarkType::NoMark`] and [`MarkType::AllMarks`].
    const fn slot(self) -> Option<usize> {
        match self {
            Self::Mark => Some(0),
            Self::Mark2 => Some(1),
            Self::Mark3 => Some(2),
            Self::MarkIo => Some(3),
            Self::MarkAssert => Some(4),
            Self::NoMark | Self::AllMarks => None,
        }
    }
}

/// Mark mask selecting every mark (`SM_MT_ALLMARKS`).
pub const ALL_MARKS: i32 = MarkType::AllMarks as i32;

/// Index of `SM_MT_MARKIO` in the mark arrays (the `default:` mark upstream).
const MARK_IO: usize = 3;

/// An installed notify callback, or none (what a [`NotifyLease`] carries).
pub type BoxedCallback = Option<Box<dyn NotifyCallback>>;

/// One call to each installed observer. Owner links and notifications share their order.
#[derive(Debug)]
pub(super) enum CallbackEvent {
    Notify(
        object::NotifyOperation,
        object::ObjectRef,
        [Option<object::ObjectRef>; 3],
    ),
    OwnerLink(ObjectKey, Option<object::ObjectRef>),
    BoundaryEdit(Box<object::BoundaryEdit>),
}

impl CallbackEvent {
    fn send(&self, callback: &mut dyn NotifyCallback) {
        match self {
            Self::Notify(operation, caller, [a, b, c]) => {
                let _status = callback.execute(*operation, *caller, *a, *b, *c);
            }
            Self::OwnerLink(object, owner) => callback.owner_link(*object, *owner),
            Self::BoundaryEdit(event) => callback.boundary_edit(**event),
        }
    }
}

/// Operation-local metadata events, separate from original callback ownership.
/// Both operands share this stream; no object identity is allocated by the relay.
#[derive(Clone, Debug)]
pub(crate) struct MetadataRelay {
    work: ProofWork,
    state: Arc<Mutex<MetadataRelayState>>,
}

#[derive(Debug, Default)]
struct MetadataRelayState {
    events: Vec<CallbackEvent>,
    capacity: usize,
}

impl MetadataRelay {
    /// Reserves fixed relay ownership before allocating its shared state.
    pub(crate) fn new(work: ProofWork) -> Option<Self> {
        work.charge(Some(128)).then(|| Self {
            work,
            state: Arc::new(Mutex::new(MetadataRelayState::default())),
        })
    }

    fn same_stream(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    fn disable(&self) {
        self.work.disable();
        if let Ok(mut state) = self.state.lock() {
            // Every retained slot prepaid its later move/drop work.
            state.events.clear();
        }
    }

    fn record(&self, event: &CallbackEvent) {
        if self.work.is_disabled() {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            self.work.disable();
            return;
        };
        // A fixed event copy includes the boxed typed payload, when present.
        if !self.work.charge(Some(object::BOUNDARY_EVENT_WORK as u64)) {
            state.events.clear();
            return;
        }
        if state.events.len() == state.capacity {
            let capacity = state.capacity.max(4).checked_mul(2);
            let cost = capacity
                .and_then(|n| n.checked_mul(object::BOUNDARY_EVENT_WORK))
                .and_then(|n| n.checked_add(state.events.len()));
            if !self.work.charge(cost.and_then(|n| u64::try_from(n).ok())) {
                state.events.clear();
                return;
            }
            let Some(capacity) = capacity else {
                state.events.clear();
                return;
            };
            let additional = capacity - state.events.len();
            if state.events.try_reserve_exact(additional).is_err() {
                self.work.disable();
                state.events.clear();
                return;
            }
            state.capacity = capacity;
        }
        let copied = match event {
            CallbackEvent::Notify(operation, caller, data) => {
                CallbackEvent::Notify(*operation, *caller, *data)
            }
            CallbackEvent::OwnerLink(object, owner) => CallbackEvent::OwnerLink(*object, *owner),
            CallbackEvent::BoundaryEdit(edit) => CallbackEvent::BoundaryEdit(Box::new(**edit)),
        };
        state.events.push(copied);
    }

    /// Delivery uses Context's existing pending/callback pipeline, outside locks.
    pub(crate) fn drain_into(&self, context: &mut Context) {
        if !context
            .proof_work()
            .is_some_and(|work| work.same_operation(&self.work))
        {
            self.disable();
            return;
        }
        let events = {
            let Ok(mut state) = self.state.lock() else {
                self.work.disable();
                return;
            };
            if !self.work.charge(
                u64::try_from(state.events.len())
                    .ok()
                    .and_then(|n| n.checked_mul(2)),
            ) {
                state.events.clear();
                return;
            }
            if let Some(pending) = &mut context.pending_notifications {
                // The moved events must not cause uncharged growth in the destination.
                let cost = pending
                    .len()
                    .checked_add(state.events.len())
                    .and_then(|n| n.checked_mul(object::BOUNDARY_EVENT_WORK))
                    .and_then(|n| n.checked_add(pending.len()));
                if !self.work.charge(cost.and_then(|n| u64::try_from(n).ok()))
                    || pending.try_reserve_exact(state.events.len()).is_err()
                {
                    self.work.disable();
                    state.events.clear();
                    return;
                }
            }
            state.capacity = 0;
            core::mem::take(&mut state.events)
        };
        // No relay/Work lock is held across callbacks. These were already recorded
        // on emission; forwarding/replay must never record them a second time.
        for event in events {
            context.deliver_observers(event);
        }
    }
}

/// A failed/unwound atomic edit invalidates optional evidence, without refunds.
struct MetadataCheckpoint {
    relay: MetadataRelay,
    committed: bool,
}

impl MetadataCheckpoint {
    fn new(relay: MetadataRelay) -> Self {
        Self {
            relay,
            committed: false,
        }
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for MetadataCheckpoint {
    fn drop(&mut self) {
        if !self.committed {
            self.relay.disable();
        }
    }
}

/// Restores only the optional relay after all inner notification leases restore.
struct MetadataRelayScope<'a> {
    kernel: &'a mut Kernel,
    previous: Option<MetadataRelay>,
    relay: MetadataRelay,
    committed: bool,
}

impl Drop for MetadataRelayScope<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.relay.disable();
        }
        self.kernel.context.metadata_relay = self.previous.take();
    }
}

/// Receiver of topology-change notifications (`SmNotifyCallback`).
pub trait NotifyCallback: std::fmt::Debug + Send {
    /// `SmNotifyCallback::Execute`: called once for every [`object::notify`] of an
    /// object whose context holds this callback, with the notifying object
    /// (`caller`) and the operation's data objects (see
    /// [`object::NotifyOperation`] for their meaning per operation). The status is
    /// ignored by [`object::notify`].
    ///
    /// # Errors
    ///
    /// Whatever the callback reports.
    fn execute(
        &mut self,
        operation: object::NotifyOperation,
        caller: object::ObjectRef,
        data1: Option<object::ObjectRef>,
        data2: Option<object::ObjectRef>,
        data3: Option<object::ObjectRef>,
    ) -> Result<()>;

    /// This callback as [`core::any::Any`], so that whoever installed it can
    /// recognise it in a context (upstream compares callback addresses, e.g.
    /// `SmTrackTopologyChanges::IsTracking`). The default, `None`, suits callbacks
    /// nobody needs to recognise.
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        None
    }

    /// Mutable downcast for an owning observer. The default imposes no Any or
    /// static bound on existing callback implementations.
    fn as_any_mut(&mut self) -> Option<&mut dyn core::any::Any> {
        None
    }

    /// Reserves canonical architecture-independent producer-event work/storage
    /// before a deferred allocation. Old observers opt out without state changes.
    fn reserve_boundary_edit(&mut self, _logical_work: usize) -> bool {
        false
    }

    /// Observes only a successful producer's typed coverage construction.
    fn boundary_edit(&mut self, _event: object::BoundaryEdit) {}

    /// Records the object `object` is owned by (`SmCurve::GetOwner`,
    /// `SmSurface::GetOwner`, `SmEdgeuse::GetEdge`; `None` for a null owner) for the
    /// notifications that follow. An upstream callback reads these links from the
    /// notified objects themselves; an [`object::ObjectRef`] carries none, so the
    /// notifying code reports a link no notification announces (a UV trim curve's edge
    /// use, that edge use's edge) before the notification a callback needs it for
    /// ([`object::report_owner_link`]). The default ignores it.
    fn owner_link(&mut self, object: ObjectKey, owner: Option<object::ObjectRef>) {
        let _ = (object, owner);
    }
}

/// Shared modeling state (`SmContext`): traversal marks, the Boolean flag, notify
/// callbacks and the context's model size estimate and zone tolerance.
#[derive(Debug)]
pub struct Context {
    /// `m_lCurrentMark`, `m_lCurrentMark2`, `m_lCurrentMark3`, `m_lCurrentMarkIO`,
    /// `m_lCurrentMarkAssert`. Atomic so that a query holding the context shared can
    /// claim a mark ([`Context::claim_mark`]), as upstream's `const` queries do
    /// through `SmNewMarkAndLock(const SmContext*, ..)`; single-threaded use orders the
    /// accesses.
    current_marks: [AtomicU64; 5],
    /// `m_bMarkLock`, `m_bMark2Lock`, `m_bMark3Lock`, `m_bMarkIOLock`,
    /// `m_bMarkAssertLock`.
    mark_locks: [AtomicBool; 5],
    /// `m_bDoingBoolean`.
    doing_boolean: bool,
    /// `m_dThisModelSizeEstimate`.
    model_size_estimate: f64,
    /// `m_dThisLargeSmallSizeRatio`.
    large_small_size_ratio: f64,
    /// `m_sThisZoneTol3d`.
    zone_tol_3d: f64,
    /// `m_pUserNotifyCallback`.
    user_notify_callback: Option<Box<dyn NotifyCallback>>,
    /// `m_pSysNotifyCallBack`.
    sys_notify_callback: Option<Box<dyn NotifyCallback>>,
    /// Calls awaiting the outcome of an atomic edit, lent alongside the callbacks.
    pending_notifications: Option<Vec<CallbackEvent>>,
    /// Optional observed operation fuel, without borrowing its callback.
    proof_work: Option<ProofWork>,
    /// Optional operation observer, lent with callbacks but never query metadata.
    metadata_relay: Option<MetadataRelay>,
    /// This context's notification identity domain (`Context::geometry_key`,
    /// `Context::brep_serial`), lent with the callbacks ([`NotifyLease`]).
    identities: IdentityDomain,
}

/// The reserved range of notification keys: the top bit set. Only the kernel issues
/// keys in it: topology keys ([`TOPOLOGY_KEY_BASE`], a B-rep serial, the object's
/// kind and its arena index) and geometry keys ([`GEOMETRY_KEY_BASE`]). A caller's own
/// labels stay below it ([`is_reserved_key`]).
pub const KERNEL_KEY_BASE: u64 = 0b10 << 62;

/// The kernel's topology keys: `0b10 << 62 | serial << 36 | kind << 32 | index`
/// (`topology::brep::p3::topology_ref`).
pub const TOPOLOGY_KEY_BASE: u64 = 0b10 << 62;

/// The kernel's geometry keys: both top bits set, then a counter of the issuing
/// domain.
pub const GEOMETRY_KEY_BASE: u64 = 0b11 << 62;

/// The largest B-rep serial a domain issues (26 bits, bits 36..62 of a topology key).
pub const MAX_BREP_SERIAL: u64 = (1 << 26) - 1;

/// Whether `key` is in the kernel's reserved range ([`KERNEL_KEY_BASE`]).
#[must_use]
pub const fn is_reserved_key(key: ObjectKey) -> bool {
    key.0 >= KERNEL_KEY_BASE
}

/// The token of one identity domain. Each [`Context`] makes its own and tokens are
/// told apart only by `Arc::ptr_eq` (never ordered or hashed), so no two domains are
/// confused.
#[derive(Debug)]
struct DomainToken(());

/// A context's notification identity domain: its token and how many geometry keys and
/// B-rep serials it has issued.
#[derive(Debug)]
struct IdentityDomain {
    token: Arc<DomainToken>,
    geometry_issued: Arc<AtomicU64>,
    breps_issued: u64,
    /// The stand-in a crate edit left while it lends the context's callbacks
    /// ([`Context::lend_notify`]): the context issues nothing and lends nothing until
    /// they are given back.
    lent: bool,
}

impl IdentityDomain {
    fn new() -> Self {
        Self {
            token: Arc::new(DomainToken(())),
            geometry_issued: Arc::new(AtomicU64::new(0)),
            breps_issued: 0,
            lent: false,
        }
    }
}

/// A kernel-issued identity: the issuing domain and the value it issued (a geometry
/// key or a B-rep serial).
#[derive(Debug)]
struct Issued {
    domain: Arc<DomainToken>,
    value: u64,
}

/// Where an object keeps the notification identity the kernel issued it (upstream
/// passes the object itself, known by address): empty until a context issues one.
/// Only the crate fills a slot, from the context the object is notified through; an
/// identity is valid only in the domain that issued it, and any other domain issues a
/// fresh one. A clone is empty (a copy is another object), so no two live objects
/// hold one identity.
#[derive(Debug, Default)]
pub struct IdentitySlot(Option<Issued>);

impl Clone for IdentitySlot {
    /// An empty slot: the copy is a new object.
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl IdentitySlot {
    /// An empty slot.
    #[must_use]
    pub const fn new() -> Self {
        Self(None)
    }

    /// The issued value (a geometry key or a B-rep serial), once there is one.
    #[must_use]
    pub fn value(&self) -> Option<u64> {
        self.0.as_ref().map(|i| i.value)
    }

    /// The same identity in a second slot, for a [`crate::topology::Snapshot`] that stands
    /// in for this object while it is not live: the snapshot is only ever put back in
    /// place of the object ([`crate::topology::Brep::restore`]) or dropped, so no two live
    /// objects hold one identity.
    pub(crate) fn duplicate_for_snapshot(&self) -> Self {
        Self(self.0.as_ref().map(|i| Issued {
            domain: Arc::clone(&i.domain),
            value: i.value,
        }))
    }
}

/// A context's notify callbacks and identity domain, lent to another context for a
/// while ([`Context::take_notify_lease`]), so that every identity the callbacks see
/// still comes from the domain they live with.
#[derive(Debug)]
pub struct NotifyLease {
    user: BoxedCallback,
    sys: BoxedCallback,
    pending: Option<Vec<CallbackEvent>>,
    relay: Option<MetadataRelay>,
    identities: IdentityDomain,
}

/// Return the callbacks and identity domain even when the caller unwinds.
struct NotificationScope<'a> {
    kernel: &'a mut Kernel,
    notifications: Context,
}
impl Drop for NotificationScope<'_> {
    fn drop(&mut self) {
        self.kernel
            .context
            .install_lease(self.notifications.take_lease());
    }
}

/// Lend the callbacks back to the stand-in even when the caller unwinds
/// ([`Context::with_lease_returned`]).
struct ReturnedLease<'a> {
    kernel: &'a mut Kernel,
    lent: &'a mut Context,
}
impl Drop for ReturnedLease<'_> {
    fn drop(&mut self) {
        let lease = self.kernel.context.take_lease();
        self.kernel.context.identities.lent = true;
        self.lent.install_lease(lease);
    }
}

/// The refusal of a context whose callbacks a crate edit has lent out
/// ([`Context::lend_notify`]): a nested edit through it would notify no callback, in
/// another domain.
const NOTIFY_LENT: SolidError = SolidError::InvalidInput {
    operation: "geom::context::lend_notify",
    reason: "the context's notify callbacks are lent to an edit in progress (nested edit)",
};

/// The refusal of an exhausted identity domain.
const IDENTITIES_EXHAUSTED: SolidError = SolidError::InvalidInput {
    operation: "geom::context::identities",
    reason: "the context has issued every notification identity of the kind",
};

impl Default for Context {
    /// `SmContext()`: both sizes [`tol::USE_DEFAULT`].
    fn default() -> Self {
        Self::new(tol::USE_DEFAULT, tol::USE_DEFAULT)
    }
}

impl Context {
    /// The geometry key the object owning `slot` is notified under in this context:
    /// the slot's key when this context's domain issued it, else a fresh key of the
    /// domain, stored in the slot (the object enters the domain). Keys are in
    /// [`GEOMETRY_KEY_BASE`]'s range and unique among those the domain has issued.
    ///
    /// # Errors
    ///
    /// `InvalidInput` once the domain has issued every geometry key, or while a crate
    /// edit has lent the context's callbacks ([`Context::lend_notify`]).
    pub(crate) fn geometry_key(&mut self, slot: &mut IdentitySlot) -> Result<ObjectKey> {
        self.check_not_lent()?;
        if let Some(i) = &slot.0 {
            if Arc::ptr_eq(&i.domain, &self.identities.token) {
                return Ok(ObjectKey(i.value));
            }
        }
        // Only key uniqueness is shared, not model data.
        // Rust 1.99 deprecates `fetch_update` as a rename of `try_update`, which rust-version
        // 1.85 doesn't have yet, so it stays (#14360).
        #[allow(deprecated, reason = "try_update is newer than 1.85 (#14360)")]
        let n = self
            .identities
            .geometry_issued
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |issued| {
                issued
                    .checked_add(1)
                    .filter(|next| *next <= !GEOMETRY_KEY_BASE)
            })
            .map_err(|_| IDENTITIES_EXHAUSTED)?
            + 1;
        let key = GEOMETRY_KEY_BASE | n;
        slot.0 = Some(Issued {
            domain: Arc::clone(&self.identities.token),
            value: key,
        });
        Ok(ObjectKey(key))
    }

    /// Join a store-owned geometry sequence without changing this context's domain
    /// or previously issued identities. New keys are unique across the store's
    /// kernels; callbacks and identity leases retain their original domains.
    pub(crate) fn share_geometry_sequence(&mut self, sequence: Arc<AtomicU64>) -> Result<()> {
        self.check_not_lent()?;
        sequence.fetch_max(
            self.identities.geometry_issued.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.identities.geometry_issued = sequence;
        Ok(())
    }

    /// The serial the B-rep owning `slot` is notified under in this context (its
    /// objects' topology keys hold it): the slot's when this context's domain issued
    /// it, else a fresh serial of the domain, stored in the slot. Serials run from 1 to
    /// [`MAX_BREP_SERIAL`].
    ///
    /// # Errors
    ///
    /// `InvalidInput` once the domain has issued every serial, or while a crate edit
    /// has lent the context's callbacks ([`Context::lend_notify`]).
    pub(crate) fn brep_serial(&mut self, slot: &mut IdentitySlot) -> Result<u64> {
        self.check_not_lent()?;
        if let Some(serial) = self.issued_brep_serial(slot) {
            return Ok(serial);
        }
        let serial = self.fresh_serial()?;
        slot.0 = Some(Issued {
            domain: Arc::clone(&self.identities.token),
            value: serial,
        });
        Ok(serial)
    }

    /// The serial this context's domain issued the B-rep owning `slot`, if it did.
    pub(crate) fn issued_brep_serial(&self, slot: &IdentitySlot) -> Option<u64> {
        slot.0
            .as_ref()
            .filter(|i| Arc::ptr_eq(&i.domain, &self.identities.token))
            .map(|i| i.value)
    }

    fn fresh_serial(&mut self) -> Result<u64> {
        let n = self.identities.breps_issued + 1;
        if n > MAX_BREP_SERIAL {
            return Err(IDENTITIES_EXHAUSTED);
        }
        self.identities.breps_issued = n;
        Ok(n)
    }

    /// Borrow notification callbacks alongside the live kernel without moving
    /// the kernel's traversal marks or tolerances. Notification APIs use mutable
    /// Context while geometry APIs borrow Kernel; both refer to one upstream
    /// context. The callbacks travel with their identity domain
    /// ([`Context::lend_notify`]), and are restored on returned errors and unwinding callbacks as well.
    /// The snapshot `Context` is read-only apart from its callbacks: mark, mark-lock
    /// and doing-Boolean changes the action makes to it are dropped on return.
    ///
    /// # Errors
    ///
    /// `InvalidInput`, before anything moves, when they are already lent (a nested
    /// lend would notify nobody, in another domain).
    pub(crate) fn with_notification_context<T>(
        k: &mut Kernel,
        action: impl FnOnce(&mut Kernel, &mut Context) -> T,
    ) -> Result<T> {
        Self::with_lent_notifications(k, k.context.metadata_copy(), action)
    }

    /// Lend the callbacks to the given stand-in, returning them on unwind as well.
    pub(crate) fn with_lent_notifications<T>(
        k: &mut Kernel,
        mut notifications: Self,
        action: impl FnOnce(&mut Kernel, &mut Self) -> T,
    ) -> Result<T> {
        notifications.install_lease(k.context.lend_notify()?);
        let mut scope = NotificationScope {
            kernel: k,
            notifications,
        };
        Ok(action(scope.kernel, &mut scope.notifications))
    }

    /// Gives the callbacks and identity domain `lent` holds (the stand-in of
    /// [`Context::with_notification_context`]) back to the kernel context for
    /// `action`, and lends them to `lent` again afterwards, also when `action`
    /// unwinds: upstream has one context, so an edit nested in the lend notifies
    /// and names through it like any other. With no stand-in, or a kernel context
    /// that is not lent, `action` runs as is.
    pub(crate) fn with_lease_returned<T>(
        k: &mut Kernel,
        lent: Option<&mut Self>,
        action: impl FnOnce(&mut Kernel) -> T,
    ) -> T {
        let Some(lent) = lent.filter(|_| k.context.identities.lent) else {
            return action(k);
        };
        k.context.install_lease(lent.take_lease());
        let scope = ReturnedLease { kernel: k, lent };
        action(scope.kernel)
    }

    /// An immutable metadata snapshot, without copying callback ownership.
    pub(crate) fn metadata_copy(&self) -> Self {
        Self {
            current_marks: self
                .current_marks
                .each_ref()
                .map(|m| AtomicU64::new(m.load(Ordering::Relaxed))),
            mark_locks: self
                .mark_locks
                .each_ref()
                .map(|l| AtomicBool::new(l.load(Ordering::Relaxed))),
            doing_boolean: self.doing_boolean,
            model_size_estimate: self.model_size_estimate,
            large_small_size_ratio: self.large_small_size_ratio,
            zone_tol_3d: self.zone_tol_3d,
            user_notify_callback: None,
            sys_notify_callback: None,
            pending_notifications: None,
            proof_work: None,
            metadata_relay: None,
            identities: IdentityDomain::new(),
        }
    }

    /// Creates a context (`SmContext::SmContext(double, double)`): every mark at 1 and
    /// unlocked, not in a Boolean, no callbacks; the zone tolerance follows from the
    /// model size estimate as in [`Context::set_this_model_size_estimate`]. Pass
    /// [`tol::USE_DEFAULT`] for the system values.
    #[must_use]
    pub fn new(model_size_estimate: f64, large_small_size_ratio: f64) -> Self {
        Self {
            current_marks: [1; 5].map(AtomicU64::new),
            mark_locks: [false; 5].map(AtomicBool::new),
            doing_boolean: false,
            model_size_estimate,
            large_small_size_ratio,
            zone_tol_3d: tol::size_estimate_to_zone_tol_3d(model_size_estimate),
            user_notify_callback: None,
            sys_notify_callback: None,
            pending_notifications: None,
            proof_work: None,
            metadata_relay: None,
            identities: IdentityDomain::new(),
        }
    }

    /// Whether a Boolean operation is running (`GetDoingBoolean`).
    #[must_use]
    pub const fn doing_boolean(&self) -> bool {
        self.doing_boolean
    }

    /// Mutable access to the Boolean flag (`GetDoingBooleanRef`).
    pub fn doing_boolean_mut(&mut self) -> &mut bool {
        &mut self.doing_boolean
    }

    /// Sets the Boolean flag (`SetDoingBoolean`).
    pub fn set_doing_boolean(&mut self, doing_boolean: bool) {
        self.doing_boolean = doing_boolean;
    }

    /// Attaches the same optional owner without moving a notification callback.
    pub(crate) fn set_proof_work(&mut self, work: Option<ProofWork>) {
        self.proof_work = work;
    }

    /// The observed operation's allowance; metadata copies deliberately omit it.
    pub(crate) fn proof_work(&self) -> Option<&ProofWork> {
        self.proof_work.as_ref()
    }

    /// Producer opt-in while original Kernel callbacks remain installed.
    pub(crate) fn reserve_relay_boundary_edit(&self, work: usize) -> bool {
        self.metadata_relay
            .as_ref()
            .is_some_and(|relay| relay.work.charge(u64::try_from(work).ok()))
    }

    /// A relay-only producer must not notify original callbacks that opted out.
    pub(super) fn notify_relay_boundary_edit(&mut self, event: object::BoundaryEdit) {
        if let Some(relay) = &self.metadata_relay {
            if !relay.work.is_disabled() {
                // Producer storage was reserved before this temporary payload.
                relay.record(&CallbackEvent::BoundaryEdit(Box::new(event)));
            }
        }
    }

    /// The actual planar operation borrows this metadata stream, not callbacks.
    pub(crate) fn with_metadata_relay<T>(
        k: &mut Kernel,
        relay: Option<MetadataRelay>,
        edit: impl FnOnce(&mut Kernel) -> Result<T>,
    ) -> Result<T> {
        let Some(relay) = relay else {
            return edit(k);
        };
        if k.context
            .metadata_relay
            .as_ref()
            .is_some_and(|old| !old.same_stream(&relay))
        {
            // A different nested observer cannot replace the outer operation.
            relay.disable();
            return edit(k);
        }
        let previous = k.context.metadata_relay.replace(relay.clone());
        let mut scope = MetadataRelayScope {
            kernel: k,
            previous,
            relay,
            committed: false,
        };
        let result = edit(scope.kernel);
        scope.committed = result.is_ok();
        result
    }

    /// The application's notify callback (`GetUserNotifyCallback`).
    #[must_use]
    pub fn user_notify_callback(&self) -> Option<&dyn NotifyCallback> {
        self.user_notify_callback.as_deref()
    }

    /// The application's notify callback, for calling it (`GetUserNotifyCallback`
    /// as used by `SmObject::Notify`).
    pub fn user_notify_callback_mut(&mut self) -> Option<&mut (dyn NotifyCallback + 'static)> {
        self.user_notify_callback.as_deref_mut()
    }

    /// Sets the application's notify callback (`SetUserNotifyCallback`).
    pub fn set_user_notify_callback(&mut self, callback: Option<Box<dyn NotifyCallback>>) {
        self.user_notify_callback = callback;
    }

    /// The system notify callback (`GetSysNotifyCallback`).
    #[must_use]
    pub fn sys_notify_callback(&self) -> Option<&dyn NotifyCallback> {
        self.sys_notify_callback.as_deref()
    }

    /// The system notify callback, for calling it (`GetSysNotifyCallback` as used
    /// by `SmObject::Notify`).
    pub fn sys_notify_callback_mut(&mut self) -> Option<&mut (dyn NotifyCallback + 'static)> {
        self.sys_notify_callback.as_deref_mut()
    }

    /// Sets the system notify callback (`SetSysNotifyCallback`; upstream only warns
    /// in debug builds when one is already set).
    pub fn set_sys_notify_callback(&mut self, callback: Option<Box<dyn NotifyCallback>>) {
        self.sys_notify_callback = callback;
    }

    pub(super) fn notify_observers(&mut self, event: CallbackEvent) {
        if let Some(relay) = &self.metadata_relay {
            relay.record(&event);
        }
        self.deliver_observers(event);
    }

    /// Existing callback delivery, also used for replay of already-recorded events.
    fn deliver_observers(&mut self, event: CallbackEvent) {
        if let Some(pending) = &mut self.pending_notifications {
            pending.push(event);
            return;
        }
        if let Some(user) = self.user_notify_callback_mut() {
            event.send(user);
        }
        if let Some(system) = self.sys_notify_callback_mut() {
            event.send(system);
        }
    }

    /// Run an atomic edit with its usual identity domain, topology records and attributes.
    /// Publish its observer calls in order only after it commits. A refused or unwound
    /// edit discards its calls and returns the enclosing notification scope, if any.
    pub(crate) fn with_committed_notifications<T>(
        k: &mut Kernel,
        edit: impl FnOnce(&mut Kernel) -> Result<T>,
    ) -> Result<T> {
        use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
        let checkpoint = k
            .context
            .metadata_relay
            .clone()
            .map(MetadataCheckpoint::new);
        if k.context.user_notify_callback.is_none() && k.context.sys_notify_callback.is_none() {
            let result = edit(k);
            if result.is_ok() {
                if let Some(checkpoint) = checkpoint {
                    checkpoint.commit();
                }
            }
            return result;
        }
        let outer = k.context.pending_notifications.replace(Vec::new());
        let outcome = catch_unwind(AssertUnwindSafe(|| edit(k)));
        let events = std::mem::replace(&mut k.context.pending_notifications, outer);
        let value = match outcome {
            Ok(result) => result?,
            Err(panic) => resume_unwind(panic),
        };
        for event in events.into_iter().flatten() {
            k.context.deliver_observers(event);
        }
        if let Some(checkpoint) = checkpoint {
            checkpoint.commit();
        }
        Ok(value)
    }

    /// Takes the user and the system notify callbacks out with this context's identity
    /// domain, leaving no callbacks and a fresh domain, so that a caller can lend them to
    /// another context for a while ([`Context::restore_notify_lease`]; no upstream
    /// counterpart: a context reached through `GetContext` cannot be borrowed twice).
    /// The identities the callbacks see keep coming from their own domain.
    ///
    /// Until the lease is given back this context is a new identity domain, whose
    /// identities may coincide with the lent domain's: every caller store keyed by
    /// identities (notification records and attributes, a tracker, a callback's state)
    /// must be scoped to one domain. A lease that is never given back leaves the context
    /// a new domain for good.
    ///
    /// # Errors
    ///
    /// `InvalidInput`, before anything moves, while a crate edit has lent this context's
    /// callbacks (`Context::lend_notify`): swapping the lent context's placeholder would
    /// let a nested edit through, notifying nobody.
    pub fn take_notify_lease(&mut self) -> Result<NotifyLease> {
        self.check_not_lent()?;
        Ok(self.take_lease())
    }

    /// Installs a lease's callbacks and identity domain, dropping this context's own
    /// callbacks and domain (returning a lease to the context it came from, or lending
    /// it to a stand-in); take them out first to keep them.
    ///
    /// # Errors
    ///
    /// `InvalidInput`, before anything moves, while a crate edit has lent this context's
    /// callbacks: the edit's own return of them would drop what this installs. (Callbacks
    /// set on a lent context with the `set_*` setters are dropped the same way when the
    /// edit gives the lent ones back.)
    pub fn restore_notify_lease(&mut self, lease: NotifyLease) -> Result<()> {
        self.check_not_lent()?;
        self.install_lease(lease);
        Ok(())
    }

    /// [`Context::take_notify_lease`] without the lent check: a crate lending helper
    /// taking its stand-in's callbacks back.
    pub(crate) fn take_lease(&mut self) -> NotifyLease {
        NotifyLease {
            user: self.user_notify_callback.take(),
            sys: self.sys_notify_callback.take(),
            pending: self.pending_notifications.take(),
            relay: self.metadata_relay.take(),
            identities: core::mem::replace(&mut self.identities, IdentityDomain::new()),
        }
    }

    /// [`Context::restore_notify_lease`] without the lent check: a crate lending helper
    /// installing a lease on its stand-in, or giving a lease back to the context that
    /// lent it (which ends the lend).
    pub(crate) fn install_lease(&mut self, lease: NotifyLease) {
        self.user_notify_callback = lease.user;
        self.sys_notify_callback = lease.sys;
        self.pending_notifications = lease.pending;
        self.metadata_relay = lease.relay;
        self.identities = lease.identities;
    }

    /// Lends the callbacks and the identity domain for a crate edit, as
    /// [`Context::take_notify_lease`], and marks this context as lent until
    /// [`Context::install_lease`] gives them back.
    ///
    /// # Errors
    ///
    /// `InvalidInput` when they are already lent (a nested edit), before anything
    /// moves.
    pub(crate) fn lend_notify(&mut self) -> Result<NotifyLease> {
        self.check_not_lent()?;
        let lease = self.take_lease();
        self.identities.lent = true;
        Ok(lease)
    }

    /// Whether a crate edit has lent this context's callbacks ([`Context::lend_notify`]):
    /// a nested edit through it is refused.
    pub(crate) const fn notify_lent(&self) -> bool {
        self.identities.lent
    }

    /// `InvalidInput` when a crate edit has lent this context's callbacks
    /// ([`Context::lend_notify`]).
    pub(crate) const fn check_not_lent(&self) -> Result<()> {
        if self.notify_lent() {
            Err(NOTIFY_LENT)
        } else {
            Ok(())
        }
    }

    /// The context's model size estimate, or the system one when it is
    /// [`tol::USE_DEFAULT`] (`GetThisModelSizeEstimate`).
    #[must_use]
    pub fn this_model_size_estimate(&self, k: &Kernel) -> f64 {
        if self.model_size_estimate == tol::USE_DEFAULT {
            tol::model_size_estimate(k)
        } else {
            self.model_size_estimate
        }
    }

    /// Stores a model size estimate and the zone tolerance it implies
    /// ([`tol::size_estimate_to_zone_tol_3d`]); returns that zone tolerance
    /// (`SetThisModelSizeEstimate`).
    pub fn set_this_model_size_estimate(&mut self, model_size_estimate: f64) -> f64 {
        self.model_size_estimate = model_size_estimate;
        self.zone_tol_3d = tol::size_estimate_to_zone_tol_3d(self.model_size_estimate);
        self.zone_tol_3d
    }

    /// Stores the large/small size ratio (`SetThisLargeSmallSizeRatio`).
    pub fn set_this_large_small_size_ratio(&mut self, large_small_size_ratio: f64) {
        self.large_small_size_ratio = large_small_size_ratio;
    }

    /// The stored model size estimate, possibly [`tol::USE_DEFAULT`]
    /// (`m_dThisModelSizeEstimate`).
    #[must_use]
    pub const fn stored_model_size_estimate(&self) -> f64 {
        self.model_size_estimate
    }

    /// The stored large/small size ratio, possibly [`tol::USE_DEFAULT`]
    /// (`m_dThisLargeSmallSizeRatio`).
    #[must_use]
    pub const fn stored_large_small_size_ratio(&self) -> f64 {
        self.large_small_size_ratio
    }

    /// The context's zone tolerance (`m_sThisZoneTol3d`).
    #[must_use]
    pub const fn this_zone_tol_3d(&self) -> f64 {
        self.zone_tol_3d
    }

    /// The current value of a mark (`GetCurrentMark`): an object is marked when its
    /// mark equals this. [`MarkType::NoMark`] gives one more than the largest mark, so
    /// nothing is marked; [`MarkType::AllMarks`] gives the IO mark.
    #[must_use]
    pub fn current_mark(&self, mark_type: MarkType) -> u64 {
        match mark_type {
            MarkType::NoMark => {
                let [m1, m2, m3, mio, massert] = self
                    .current_marks
                    .each_ref()
                    .map(|m| m.load(Ordering::Relaxed));
                1 + max5(m1, m2, m3, mio, massert)
            }
            _ => self
                .current_marks
                .get(mark_type.slot().unwrap_or(MARK_IO))
                .map_or(0, |m| m.load(Ordering::Relaxed)),
        }
    }

    /// Advances a mark by two and returns the new value (`SmContext::NewMark`; the
    /// lock check is a debug-only assertion upstream). [`MarkType::NoMark`] changes
    /// nothing and returns [`Context::current_mark`] of it.
    fn new_mark(&self, mark_type: MarkType) -> u64 {
        if mark_type == MarkType::NoMark {
            return self.current_mark(MarkType::NoMark);
        }
        self.current_marks
            .get(mark_type.slot().unwrap_or(MARK_IO))
            .map_or(0, |m| m.fetch_add(2, Ordering::Relaxed).wrapping_add(2))
    }

    /// Whether a mark is locked (`IsMarkLocked`); never for [`MarkType::NoMark`] or
    /// [`MarkType::AllMarks`].
    #[must_use]
    pub fn is_mark_locked(&self, mark_type: MarkType) -> bool {
        mark_type
            .slot()
            .and_then(|s| self.mark_locks.get(s))
            .is_some_and(|l| l.load(Ordering::Relaxed))
    }

    /// Sets a mark's lock flag (no-op for marks without a slot).
    fn set_lock(&self, mark_type: MarkType, locked: bool) {
        if let Some(l) = mark_type.slot().and_then(|s| self.mark_locks.get(s)) {
            l.store(locked, Ordering::Relaxed);
        }
    }

    /// `SmNewMarkAndLock(const SmContext*, mask)` for a holder of the context that
    /// cannot change it otherwise (upstream's `const` queries): picks the first
    /// unlocked mark allowed by `mask` as [`MarkLock::new`] does, advances and locks
    /// it until the returned guard is dropped (`~SmNewMarkAndLock`).
    #[must_use]
    pub fn claim_mark(&self, mask: i32) -> SharedMarkLock<'_> {
        let mark_type = unlocked_mark(self, mask);
        self.new_mark(mark_type);
        self.set_lock(mark_type, true);
        SharedMarkLock {
            context: self,
            mark_type,
        }
    }
}

/// A mark claimed through a shared context ([`Context::claim_mark`]); dropping it
/// unlocks the mark if it is still locked (`~SmNewMarkAndLock`).
#[derive(Debug)]
pub struct SharedMarkLock<'a> {
    context: &'a Context,
    mark_type: MarkType,
}

impl SharedMarkLock<'_> {
    /// The claimed mark (`GetMarkType`).
    #[must_use]
    pub const fn mark_type(&self) -> MarkType {
        self.mark_type
    }
}

impl Drop for SharedMarkLock<'_> {
    fn drop(&mut self) {
        if self.context.is_mark_locked(self.mark_type) {
            self.context.set_lock(self.mark_type, false);
        }
    }
}

/// `smos_5Max` = `smos_Max(smos_3Max(a, b, c), smos_Max(d, e))`.
fn max5(a: u64, b: u64, c: u64, d: u64, e: u64) -> u64 {
    a.max(b).max(c).max(d.max(e))
}

/// `SmNewMarkAndLock::LockMark`.
fn lock_mark(ctx: &mut Context, mark_type: MarkType) {
    ctx.set_lock(mark_type, true);
}

/// `SmNewMarkAndLock::UnlockMark`.
fn unlock_mark(ctx: &mut Context, mark_type: MarkType) {
    ctx.set_lock(mark_type, false);
}

/// The first unlocked mark selected by `mask` in the order mark, mark2, mark3, IO,
/// assert; if all are locked, the mask itself when it names a single mark, else the IO
/// mark (`SmNewMarkAndLock::GetUnlockedMark`).
fn unlocked_mark(ctx: &Context, mask: i32) -> MarkType {
    const ORDER: [MarkType; 5] = [
        MarkType::Mark,
        MarkType::Mark2,
        MarkType::Mark3,
        MarkType::MarkIo,
        MarkType::MarkAssert,
    ];
    if let Some(m) = ORDER
        .iter()
        .find(|m| (mask & m.code()) != 0 && !ctx.is_mark_locked(**m))
    {
        return *m;
    }
    ORDER
        .iter()
        .find(|m| mask == m.code())
        .copied()
        .unwrap_or(MarkType::MarkIo)
}

/// `InvalidInput` for a context index outside the caller's slice.
fn index_error(operation: &'static str) -> SolidError {
    SolidError::InvalidInput {
        operation,
        reason: "context index out of range",
    }
}

/// The context at `index` in `contexts`.
fn ctx_at<'a>(
    contexts: &'a mut [Context],
    index: usize,
    operation: &'static str,
) -> Result<&'a mut Context> {
    contexts
        .get_mut(index)
        .ok_or_else(|| index_error(operation))
}

/// Fails when a bound context index is outside `contexts`.
fn check_index(contexts: &[Context], index: usize, operation: &'static str) -> Result<()> {
    if index < contexts.len() {
        Ok(())
    } else {
        Err(index_error(operation))
    }
}

/// A claimed traversal mark (`SmNewMarkAndLock`): advances an unlocked mark of one or
/// two contexts and locks it until [`MarkLock::release`].
///
/// Contexts are given as indices into a caller-owned `&mut [Context]` (see the module
/// deviations); pass the same slice to every method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkLock {
    context1: Option<usize>,
    context2: Option<usize>,
    mark_type1: MarkType,
    mark_type2: MarkType,
}

impl Default for MarkLock {
    fn default() -> Self {
        Self::unbound()
    }
}

impl MarkLock {
    /// A lock bound to no context (`SmNewMarkAndLock()`).
    #[must_use]
    pub const fn unbound() -> Self {
        Self {
            context1: None,
            context2: None,
            mark_type1: MarkType::NoMark,
            mark_type2: MarkType::NoMark,
        }
    }

    /// Picks the first unlocked mark of `contexts[ctx]` allowed by `mask` (usually
    /// [`ALL_MARKS`]), advances and locks it (`SmNewMarkAndLock(const SmContext*,
    /// SmBitArray)`).
    pub fn new(contexts: &mut [Context], ctx: usize, mask: i32) -> Result<Self> {
        const OP: &str = "geom::context::MarkLock::new";
        let c = ctx_at(contexts, ctx, OP)?;
        let mark_type1 = unlocked_mark(c, mask);
        c.new_mark(mark_type1);
        lock_mark(c, mark_type1);
        Ok(Self {
            context1: Some(ctx),
            context2: None,
            mark_type1,
            mark_type2: MarkType::NoMark,
        })
    }

    /// Claims a mark in each of two contexts, e.g. the two operands of a Boolean
    /// (`SmNewMarkAndLock(SmContext*, SmContext*)`).
    ///
    /// When `ctx1 == ctx2` both marks are `mark_type1`, advanced once in `contexts[ctx1]`.
    /// Otherwise `mark_type2` is picked from `contexts[ctx2]` and advanced and locked
    /// there, so each context is advanced once on its own claimed mark and
    /// [`MarkLock::release`] leaves no stray lock (`WR-1`).
    pub fn new_pair(contexts: &mut [Context], ctx1: usize, ctx2: usize) -> Result<Self> {
        const OP: &str = "geom::context::MarkLock::new_pair";
        check_index(contexts, ctx2, OP)?;
        let c1 = ctx_at(contexts, ctx1, OP)?;
        let mark_type1 = unlocked_mark(c1, ALL_MARKS);
        c1.new_mark(mark_type1);
        lock_mark(c1, mark_type1);
        let mark_type2 = if ctx1 == ctx2 {
            mark_type1
        } else {
            let c2 = ctx_at(contexts, ctx2, OP)?;
            let m2 = unlocked_mark(c2, ALL_MARKS);
            c2.new_mark(m2);
            lock_mark(c2, m2);
            m2
        };
        Ok(Self {
            context1: Some(ctx1),
            context2: Some(ctx2),
            mark_type1,
            mark_type2,
        })
    }

    /// Unlocks the claimed marks that are still locked and unbinds the lock
    /// (`~SmNewMarkAndLock`).
    pub fn release(&mut self, contexts: &mut [Context]) -> Result<()> {
        const OP: &str = "geom::context::MarkLock::release";
        if let Some(i) = self.context1 {
            let c = ctx_at(contexts, i, OP)?;
            if c.is_mark_locked(self.mark_type1) {
                unlock_mark(c, self.mark_type1);
            }
        }
        if let Some(i) = self.context2 {
            let c = ctx_at(contexts, i, OP)?;
            if c.is_mark_locked(self.mark_type2) {
                unlock_mark(c, self.mark_type2);
            }
        }
        *self = Self::unbound();
        Ok(())
    }

    /// The (first) claimed mark (`GetMarkType`).
    #[must_use]
    pub const fn mark_type(&self) -> MarkType {
        self.mark_type1
    }

    /// The mark claimed in the first context (`GetMarkType1`).
    #[must_use]
    pub const fn mark_type1(&self) -> MarkType {
        self.mark_type1
    }

    /// The mark claimed in the second context (`GetMarkType2`).
    #[must_use]
    pub const fn mark_type2(&self) -> MarkType {
        self.mark_type2
    }

    /// Index of the (first) bound context (`GetContext`).
    #[must_use]
    pub const fn context(&self) -> Option<usize> {
        self.context1
    }

    /// Index of the second bound context, if any (`GetContext2`).
    #[must_use]
    pub const fn context2(&self) -> Option<usize> {
        self.context2
    }

    /// Advances the claimed mark(s) while keeping them locked, invalidating every
    /// mark set so far (`SmNewMarkAndLock::NewMark`).
    pub fn new_mark(&self, contexts: &mut [Context]) -> Result<()> {
        const OP: &str = "geom::context::MarkLock::new_mark";
        let i1 = self.context1.ok_or(SolidError::InvalidInput {
            operation: OP,
            reason: "no context bound",
        })?;
        ctx_at(contexts, i1, OP)?.new_mark(self.mark_type1);
        if let Some(i2) = self.context2 {
            if i1 != i2 {
                ctx_at(contexts, i2, OP)?.new_mark(self.mark_type2);
            }
        }
        Ok(())
    }

    /// Rebinds the first context: unlocks the old first mark, then claims and locks
    /// (without advancing) the first unlocked mark of `contexts[ctx]` allowed by
    /// `mask`; returns it (`SmNewMarkAndLock::SetContext(SmContext*, SmMarkType)`).
    /// The second context binding is kept.
    pub fn set_context(
        &mut self,
        contexts: &mut [Context],
        ctx: usize,
        mask: i32,
    ) -> Result<MarkType> {
        const OP: &str = "geom::context::MarkLock::set_context";
        check_index(contexts, ctx, OP)?;
        if let Some(i) = self.context1 {
            if self.mark_type1 != MarkType::NoMark {
                unlock_mark(ctx_at(contexts, i, OP)?, self.mark_type1);
            }
        }
        self.context1 = Some(ctx);
        let c = ctx_at(contexts, ctx, OP)?;
        self.mark_type1 = unlocked_mark(c, mask);
        lock_mark(c, self.mark_type1);
        Ok(self.mark_type1)
    }

    /// Rebinds both contexts: unlocks the old marks, then claims and locks (without
    /// advancing) the first unlocked mark of each new context; returns the first
    /// (`SmNewMarkAndLock::SetContext(SmContext*, SmContext*)`).
    pub fn set_context_pair(
        &mut self,
        contexts: &mut [Context],
        ctx1: usize,
        ctx2: usize,
    ) -> Result<MarkType> {
        const OP: &str = "geom::context::MarkLock::set_context_pair";
        check_index(contexts, ctx1, OP)?;
        check_index(contexts, ctx2, OP)?;
        if let Some(i) = self.context1 {
            if self.mark_type1 != MarkType::NoMark {
                unlock_mark(ctx_at(contexts, i, OP)?, self.mark_type1);
            }
        }
        if let Some(i2) = self.context2 {
            if self.context1 != Some(i2) && self.mark_type2 != MarkType::NoMark {
                unlock_mark(ctx_at(contexts, i2, OP)?, self.mark_type2);
            }
        }
        self.context1 = Some(ctx1);
        self.context2 = Some(ctx2);
        let c1 = ctx_at(contexts, ctx1, OP)?;
        self.mark_type1 = unlocked_mark(c1, ALL_MARKS);
        lock_mark(c1, self.mark_type1);
        if ctx1 == ctx2 {
            self.mark_type2 = self.mark_type1;
        } else {
            let c2 = ctx_at(contexts, ctx2, OP)?;
            self.mark_type2 = unlocked_mark(c2, ALL_MARKS);
            lock_mark(c2, self.mark_type2);
        }
        Ok(self.mark_type1)
    }
}

/// Runs `f` with a mark of the kernel's context claimed and locked, then releases it:
/// the scoped `{ SmNewMarkAndLock sMarkLock(pContext, mask); ... }` idiom.
pub fn with_mark_lock<R>(
    k: &mut Kernel,
    mask: i32,
    f: impl FnOnce(&mut Kernel, MarkType) -> R,
) -> Result<R> {
    let mut lock = MarkLock::new(std::slice::from_mut(&mut k.context), 0, mask)?;
    let r = f(k, lock.mark_type());
    lock.release(std::slice::from_mut(&mut k.context))?;
    Ok(r)
}

/// How mass-property computations treat faces, edges and vertices
/// (`SmMassPropertyBehavior`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MassPropertyBehavior {
    /// Density of the material (`m_dDensity`, default 1).
    pub density: f64,
    /// Thickness given to faces (`m_dFaceThickness`, default 0).
    pub face_thickness: f64,
    /// Radius of the tube swept around edges (`m_dEdgeCrossSectionRadius`, default 0).
    pub edge_cross_section_radius: f64,
    /// Radius of the sphere around vertices (`m_dVertexSphereRadius`, default 0).
    pub vertex_sphere_radius: f64,
    /// Face area mode (`m_lFaceAreaBehavior`): 0 solid/void faces only, 1 every face
    /// once, 2 every face twice.
    pub face_area_behavior: u64,
    /// Face volume mode (`m_lFaceVolumeBehavior`): 0 region volumes, 1 thin shells.
    pub face_volume_behavior: u64,
}

impl Default for MassPropertyBehavior {
    fn default() -> Self {
        Self::new(1.0, 0.0, 0.0, 0.0, 0, 0)
    }
}

impl MassPropertyBehavior {
    /// Builds the options (`SmMassPropertyBehavior::SmMassPropertyBehavior`).
    #[must_use]
    pub const fn new(
        density: f64,
        face_thickness: f64,
        edge_cross_section_radius: f64,
        vertex_sphere_radius: f64,
        face_area_behavior: u64,
        face_volume_behavior: u64,
    ) -> Self {
        Self {
            density,
            face_thickness,
            edge_cross_section_radius,
            vertex_sphere_radius,
            face_area_behavior,
            face_volume_behavior,
        }
    }
}

/// Creates the default context in `slot` unless one exists (`SmApiCreateContext`).
pub fn create_context(slot: &mut Option<Context>) {
    if slot.is_none() {
        *slot = Some(Context::default());
    }
}

/// The context in `slot`, created with defaults first if empty
/// (`SmApiGetOrCreateContext`).
pub fn get_or_create_context(slot: &mut Option<Context>) -> &mut Context {
    slot.get_or_insert_with(Context::default)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_shared_context_claims_and_unlocks_a_mark() {
        let mut k = Kernel::default();
        let before = k.context.current_mark(MarkType::Mark2);
        let m = with_mark_lock(&mut k, ALL_MARKS, |k, outer| {
            assert_eq!(outer, MarkType::Mark);
            let lock = k.context.claim_mark(ALL_MARKS);
            // the first mark is locked by the enclosing lock: the second is claimed
            assert_eq!(lock.mark_type(), MarkType::Mark2);
            assert!(k.context.is_mark_locked(MarkType::Mark2));
            lock.mark_type()
        })
        .unwrap();
        assert_eq!(m, MarkType::Mark2);
        assert!(!k.context.is_mark_locked(MarkType::Mark2));
        assert_eq!(k.context.current_mark(MarkType::Mark2), before + 2);
    }

    use super::*;

    const ALL: [MarkType; 7] = [
        MarkType::NoMark,
        MarkType::Mark,
        MarkType::Mark2,
        MarkType::Mark3,
        MarkType::MarkIo,
        MarkType::MarkAssert,
        MarkType::AllMarks,
    ];

    #[test]
    fn mark_type_codes_round_trip() {
        for m in ALL {
            assert_eq!(MarkType::from_code(m.code()), Some(m));
        }
        assert_eq!(MarkType::from_code(3), None);
    }

    #[test]
    fn new_context_defaults() {
        let k = Kernel::default();
        let c = Context::default();
        for m in ALL {
            assert!(!c.is_mark_locked(m));
        }
        assert_eq!(c.current_mark(MarkType::Mark), 1);
        assert_eq!(c.current_mark(MarkType::NoMark), 2);
        assert_eq!(c.current_mark(MarkType::AllMarks), 1);
        assert!(!c.doing_boolean());
        assert_eq!(c.this_zone_tol_3d(), 1.0e-5);
        assert_eq!(c.this_model_size_estimate(&k), 50.0);
        assert!(c.user_notify_callback().is_none());
        assert!(c.sys_notify_callback().is_none());
    }

    #[test]
    fn model_size_sets_zone_tol() {
        let k = Kernel::default();
        let mut c = Context::new(1000.0, tol::USE_DEFAULT);
        assert_eq!(c.this_zone_tol_3d(), 1.0e-4);
        assert_eq!(c.this_model_size_estimate(&k), 1000.0);
        assert_eq!(c.set_this_model_size_estimate(0.01), 1.0e-6);
        assert_eq!(c.set_this_model_size_estimate(tol::USE_DEFAULT), 1.0e-5);
        assert_eq!(c.this_model_size_estimate(&k), 50.0);
        c.set_this_large_small_size_ratio(10.0);
        *c.doing_boolean_mut() = true;
        assert!(c.doing_boolean());
    }

    #[test]
    fn single_lock_cycle() {
        let mut cs = [Context::default()];
        let mut a = MarkLock::new(&mut cs, 0, ALL_MARKS).unwrap();
        assert_eq!(a.mark_type(), MarkType::Mark);
        assert_eq!(cs[0].current_mark(MarkType::Mark), 3);
        assert!(cs[0].is_mark_locked(MarkType::Mark));
        let mut b = MarkLock::new(&mut cs, 0, ALL_MARKS).unwrap();
        assert_eq!(b.mark_type(), MarkType::Mark2);
        b.new_mark(&mut cs).unwrap();
        assert_eq!(cs[0].current_mark(MarkType::Mark2), 5);
        b.release(&mut cs).unwrap();
        a.release(&mut cs).unwrap();
        assert!(!cs[0].is_mark_locked(MarkType::Mark));
        assert!(!cs[0].is_mark_locked(MarkType::Mark2));
        assert_eq!(cs[0].current_mark(MarkType::NoMark), 6);
        assert!(MarkLock::unbound().new_mark(&mut cs).is_err());
    }

    #[test]
    fn exhausted_marks_fall_back() {
        let mut cs = [Context::default()];
        let locks: Vec<MarkLock> = (0..5)
            .map(|_| MarkLock::new(&mut cs, 0, ALL_MARKS).unwrap())
            .collect();
        assert_eq!(locks[4].mark_type(), MarkType::MarkAssert);
        let x = MarkLock::new(&mut cs, 0, ALL_MARKS).unwrap();
        assert_eq!(x.mark_type(), MarkType::MarkIo);
        let y = MarkLock::new(&mut cs, 0, MarkType::Mark3.code()).unwrap();
        assert_eq!(y.mark_type(), MarkType::Mark3);
    }

    #[test]
    fn pair_lock_claims_each_context_once() {
        let mut cs = [Context::default(), Context::default()];
        let _first = MarkLock::new(&mut cs, 1, ALL_MARKS).unwrap();
        let mut p = MarkLock::new_pair(&mut cs, 0, 1).unwrap();
        assert_eq!(p.mark_type1(), MarkType::Mark);
        assert_eq!(p.mark_type2(), MarkType::Mark2);
        // each context advanced once and locked on its own mark (WR-1)
        assert_eq!(cs[0].current_mark(MarkType::Mark), 3);
        assert_eq!(cs[1].current_mark(MarkType::Mark2), 3);
        assert!(cs[1].is_mark_locked(MarkType::Mark));
        assert!(cs[1].is_mark_locked(MarkType::Mark2));
        p.release(&mut cs).unwrap();
        assert!(!cs[0].is_mark_locked(MarkType::Mark));
        assert!(!cs[1].is_mark_locked(MarkType::Mark2));
        assert!(cs[1].is_mark_locked(MarkType::Mark));
        let same = MarkLock::new_pair(&mut cs, 0, 0).unwrap();
        assert_eq!(same.mark_type2(), same.mark_type1());
        assert!(MarkLock::new_pair(&mut cs, 0, 2).is_err());
    }

    #[test]
    fn set_context_rebinds() {
        let mut cs = [Context::default(), Context::default()];
        let mut l = MarkLock::unbound();
        assert_eq!(
            l.set_context(&mut cs, 1, ALL_MARKS).unwrap(),
            MarkType::Mark
        );
        assert_eq!(cs[1].current_mark(MarkType::Mark), 1);
        assert_eq!(l.set_context_pair(&mut cs, 0, 1).unwrap(), MarkType::Mark);
        assert!(cs[0].is_mark_locked(MarkType::Mark));
        assert!(cs[1].is_mark_locked(MarkType::Mark));
        l.release(&mut cs).unwrap();
        assert!(!cs[0].is_mark_locked(MarkType::Mark));
        assert!(!cs[1].is_mark_locked(MarkType::Mark));
    }

    #[test]
    fn scoped_kernel_lock() {
        let mut k = Kernel::default();
        let m = with_mark_lock(&mut k, ALL_MARKS, |k, m| {
            assert!(k.context.is_mark_locked(m));
            m
        })
        .unwrap();
        assert_eq!(m, MarkType::Mark);
        assert!(!k.context.is_mark_locked(m));
    }

    #[test]
    fn api_context_slot() {
        let mut slot = None;
        create_context(&mut slot);
        get_or_create_context(&mut slot).set_doing_boolean(true);
        create_context(&mut slot);
        assert!(get_or_create_context(&mut slot).doing_boolean());
        let mp = MassPropertyBehavior::default();
        assert_eq!(mp.density, 1.0);
        assert_eq!(mp.face_area_behavior, 0);
    }

    #[test]
    fn wr1_two_contexts_each_advance_and_lock_their_own_mark() {
        // Case A: context 0 already holds `Mark`, so context 0 picks `Mark2` while
        // context 1 picks `Mark`. Pinned v1.0.1 advanced context 0's `Mark2` twice,
        // left context 1's `Mark` unadvanced and unlocked, and locked context 1's
        // `Mark2`, which `release` then left locked.
        {
            let mut cs = [Context::default(), Context::default()];
            let mut outer0 = MarkLock::new(&mut cs, 0, ALL_MARKS).unwrap();
            let mut pair = MarkLock::new_pair(&mut cs, 0, 1).unwrap();
            assert_eq!(pair.mark_type1(), MarkType::Mark2);
            assert_eq!(pair.mark_type2(), MarkType::Mark);
            // Each context is advanced once on its own claimed mark and locks
            // `(ctx2, mark_type2)`.
            assert_eq!(cs[0].current_mark(MarkType::Mark2), 3);
            assert!(cs[0].is_mark_locked(MarkType::Mark2));
            assert_eq!(cs[1].current_mark(MarkType::Mark), 3);
            assert!(cs[1].is_mark_locked(MarkType::Mark));
            assert_eq!(cs[1].current_mark(MarkType::Mark2), 1);
            assert!(!cs[1].is_mark_locked(MarkType::Mark2));
            pair.new_mark(&mut cs).unwrap();
            assert_eq!(cs[0].current_mark(MarkType::Mark2), 5);
            assert_eq!(cs[1].current_mark(MarkType::Mark), 5);
            pair.release(&mut cs).unwrap();
            outer0.release(&mut cs).unwrap();
            for m in ALL {
                assert!(!cs[0].is_mark_locked(m));
                assert!(!cs[1].is_mark_locked(m));
            }
        }

        // Case B (vice versa): context 1 already holds `Mark`, so context 0 picks `Mark`
        // while context 1 picks `Mark2`. Pinned v1.0.1 left `(1, Mark2)` unlocked, so a
        // nested claim on context 1 collided with `pair.mark_type2()`.
        {
            let mut cs = [Context::default(), Context::default()];
            let mut outer1 = MarkLock::new(&mut cs, 1, ALL_MARKS).unwrap();
            let mut pair = MarkLock::new_pair(&mut cs, 0, 1).unwrap();
            assert_eq!(pair.mark_type1(), MarkType::Mark);
            assert_eq!(pair.mark_type2(), MarkType::Mark2);
            assert_eq!(cs[0].current_mark(MarkType::Mark), 3);
            assert_eq!(cs[1].current_mark(MarkType::Mark2), 3);
            assert!(cs[0].is_mark_locked(MarkType::Mark));
            assert!(cs[1].is_mark_locked(MarkType::Mark));
            assert!(cs[1].is_mark_locked(MarkType::Mark2));
            let mut nested = MarkLock::new(&mut cs, 1, ALL_MARKS).unwrap();
            assert_eq!(nested.mark_type(), MarkType::Mark3);
            nested.release(&mut cs).unwrap();
            pair.release(&mut cs).unwrap();
            assert!(!cs[0].is_mark_locked(MarkType::Mark));
            assert!(!cs[1].is_mark_locked(MarkType::Mark2));
            assert!(cs[1].is_mark_locked(MarkType::Mark));
            outer1.release(&mut cs).unwrap();
            for m in ALL {
                assert!(!cs[0].is_mark_locked(m));
                assert!(!cs[1].is_mark_locked(m));
            }
        }
    }

    #[test]
    fn wr1_same_context_and_out_of_bounds() {
        {
            let mut cs = [Context::default(), Context::default()];
            let mut held = MarkLock::new(&mut cs, 1, ALL_MARKS).unwrap();
            assert_eq!(held.mark_type(), MarkType::Mark);

            let mut same0 = MarkLock::new_pair(&mut cs, 0, 0).unwrap();
            assert_eq!(same0.mark_type1(), MarkType::Mark);
            assert_eq!(same0.mark_type2(), MarkType::Mark);
            assert_eq!(same0.context(), Some(0));
            assert_eq!(same0.context2(), Some(0));
            assert_eq!(cs[0].current_mark(MarkType::Mark), 3);
            assert!(cs[0].is_mark_locked(MarkType::Mark));
            same0.release(&mut cs).unwrap();
            assert!(!cs[0].is_mark_locked(MarkType::Mark));

            let mut same1 = MarkLock::new_pair(&mut cs, 1, 1).unwrap();
            assert_eq!(same1.mark_type1(), MarkType::Mark2);
            assert_eq!(same1.mark_type2(), MarkType::Mark2);
            assert_eq!(cs[1].current_mark(MarkType::Mark2), 3);
            assert!(cs[1].is_mark_locked(MarkType::Mark2));
            same1.release(&mut cs).unwrap();
            held.release(&mut cs).unwrap();
            for m in ALL {
                assert!(!cs[0].is_mark_locked(m));
                assert!(!cs[1].is_mark_locked(m));
            }

            // Out-of-bounds indices fail without mutating any context.
            let mut fresh = [Context::default(), Context::default()];
            for (c1, c2) in [(0, 2), (2, 0), (2, 2)] {
                assert!(matches!(
                    MarkLock::new_pair(&mut fresh, c1, c2),
                    Err(SolidError::InvalidInput {
                        operation: "geom::context::MarkLock::new_pair",
                        ..
                    })
                ));
                for c in &fresh {
                    assert_eq!(c.current_mark(MarkType::Mark), 1);
                    for m in ALL {
                        assert!(!c.is_mark_locked(m));
                    }
                }
            }
            let mut empty: [Context; 0] = [];
            assert!(MarkLock::new_pair(&mut empty, 0, 0).is_err());
        }
    }
    #[test]
    fn old_observers_keep_generic_notifications_without_boundary_event_storage() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        #[derive(Debug)]
        struct OldObserver(Arc<AtomicUsize>);
        impl NotifyCallback for OldObserver {
            fn execute(
                &mut self,
                _operation: object::NotifyOperation,
                _caller: object::ObjectRef,
                _first: Option<object::ObjectRef>,
                _second: Option<object::ObjectRef>,
                _third: Option<object::ObjectRef>,
            ) -> Result<()> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let mut context = Context::default();
        context.set_user_notify_callback(Some(Box::new(OldObserver(calls.clone()))));
        context.pending_notifications = Some(Vec::new());
        let source = object::ObjectRef {
            type_code: crate::topology::edge::TYPE,
            key: ObjectKey(101),
        };
        let target = object::ObjectRef {
            key: ObjectKey(102),
            ..source
        };
        object::report_boundary_edit(
            Some(&mut context),
            object::BoundaryEdit::Copied { source, target },
        );
        assert!(
            context.pending_notifications.as_ref().unwrap().is_empty(),
            "default observers opt out before the optional boxed event can be queued"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        context.notify_observers(CallbackEvent::Notify(
            object::NotifyOperation::Copy,
            source,
            [Some(target), None, None],
        ));
        let pending = context.pending_notifications.take().unwrap();
        assert_eq!(
            pending.len(),
            1,
            "the old generic notification is retained unchanged"
        );
        for event in pending {
            event.send(context.user_notify_callback_mut().unwrap());
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn producer_event_storage_fits_the_canonical_architecture_independent_charge() {
        let payload = size_of::<object::BoundaryEdit>();
        let queued = size_of::<CallbackEvent>();
        assert!(payload + queued + 8 <= object::BOUNDARY_EVENT_WORK);
        assert_eq!(object::BOUNDARY_EVENT_WORK, 24 * 8);
    }
    #[test]
    fn optional_proof_work_clones_spend_one_operation_allowance() {
        let work = ProofWork::new(80);
        let second_direction = work.clone();
        assert!(work.charge(Some(8)));
        assert!(second_direction.charge(Some(8)));
        assert!(!work.charge(Some(1)));
        assert!(second_direction.is_disabled());
        assert!(!second_direction.charge(Some(0)));
    }

    #[test]
    fn optional_proof_work_overflow_cannot_reset_or_rebind_its_owner() {
        let work = ProofWork::new(2_000_000);
        let same_round = work.clone();
        let other_operation = ProofWork::new(2_000_000);
        assert!(work.same_operation(&same_round));
        assert!(!work.same_operation(&other_operation));
        assert!(!work.charge(None));
        assert!(!same_round.charge(Some(1)));
        assert!(other_operation.charge(Some(1)));
        assert!(matches!(
            work.stopped("optional proof"),
            SolidError::OutOfFuel { .. }
        ));
    }

    #[derive(Debug)]
    struct RelayLog(Arc<Mutex<Vec<ObjectKey>>>);

    impl NotifyCallback for RelayLog {
        fn execute(
            &mut self,
            _operation: object::NotifyOperation,
            caller: object::ObjectRef,
            _first: Option<object::ObjectRef>,
            _second: Option<object::ObjectRef>,
            _third: Option<object::ObjectRef>,
        ) -> Result<()> {
            self.0.lock().unwrap().push(caller.key);
            Ok(())
        }

        fn boundary_edit(&mut self, _event: object::BoundaryEdit) {
            self.0.lock().unwrap().push(ObjectKey(u64::MAX));
        }
    }

    fn relay_event(context: &mut Context, key: u64) {
        context.notify_observers(CallbackEvent::Notify(
            object::NotifyOperation::PreEdit,
            object::ObjectRef {
                type_code: crate::topology::edge::TYPE,
                key: ObjectKey(key),
            },
            [None; 3],
        ));
    }

    fn relay_destination(work: &ProofWork, log: &Arc<Mutex<Vec<ObjectKey>>>) -> Context {
        let mut context = Context::default();
        context.set_proof_work(Some(work.clone()));
        context.set_user_notify_callback(Some(Box::new(RelayLog(log.clone()))));
        context
    }

    #[test]
    fn metadata_relay_records_nested_commit_emissions_once_with_or_without_callbacks() {
        for old_callback in [false, true] {
            let mut k = Kernel::default();
            let old = Arc::new(Mutex::new(Vec::new()));
            if old_callback {
                k.context
                    .set_user_notify_callback(Some(Box::new(RelayLog(old.clone()))));
            }
            let callback = k
                .context
                .user_notify_callback()
                .map(|c| std::ptr::from_ref(c).cast::<()>());
            let work = ProofWork::new(32_000);
            let relay = MetadataRelay::new(work.clone()).unwrap();
            Context::with_metadata_relay(&mut k, Some(relay.clone()), |k| {
                Context::with_committed_notifications(k, |k| {
                    relay_event(&mut k.context, 1);
                    Context::with_committed_notifications(k, |k| {
                        relay_event(&mut k.context, 2);
                        Ok(())
                    })?;
                    relay_event(&mut k.context, 3);
                    Ok(())
                })
            })
            .unwrap();
            assert_eq!(
                callback,
                k.context
                    .user_notify_callback()
                    .map(|c| std::ptr::from_ref(c).cast::<()>())
            );
            assert!(k.context.metadata_relay.is_none());
            let expected = vec![ObjectKey(1), ObjectKey(2), ObjectKey(3)];
            assert_eq!(
                *old.lock().unwrap(),
                if old_callback {
                    expected.clone()
                } else {
                    vec![]
                }
            );
            let log = Arc::new(Mutex::new(Vec::new()));
            relay.drain_into(&mut relay_destination(&work, &log));
            assert_eq!(*log.lock().unwrap(), expected);
            assert!(!work.is_disabled());
        }
    }

    #[test]
    fn metadata_relay_survives_leases_and_drains_before_outer_callback_commit() {
        let mut k = Kernel::default();
        let old = Arc::new(Mutex::new(Vec::new()));
        k.context
            .set_user_notify_callback(Some(Box::new(RelayLog(old.clone()))));
        let work = ProofWork::new(32_000);
        let relay = MetadataRelay::new(work.clone()).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut destination = relay_destination(&work, &log);
        Context::with_committed_notifications(&mut k, |k| {
            Context::with_metadata_relay(k, Some(relay.clone()), |k| {
                Context::with_committed_notifications(k, |k| {
                    relay_event(&mut k.context, 11);
                    Context::with_notification_context(k, |k, notifications| {
                        assert!(k.context.metadata_relay.is_none());
                        assert!(notifications.metadata_relay.is_some());
                        assert!(notifications.metadata_copy().metadata_relay.is_none());
                        relay_event(notifications, 12);
                    })?;
                    Ok(())
                })
            })?;
            assert!(
                old.lock().unwrap().is_empty(),
                "original outer transaction remains pending"
            );
            relay.drain_into(&mut destination);
            assert_eq!(*log.lock().unwrap(), vec![ObjectKey(11), ObjectKey(12)]);
            relay_event(&mut k.context, 13);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            *old.lock().unwrap(),
            vec![ObjectKey(11), ObjectKey(12), ObjectKey(13)]
        );
        assert_eq!(*log.lock().unwrap(), vec![ObjectKey(11), ObjectKey(12)]);
        assert!(k.context.metadata_relay.is_none());
    }

    #[test]
    fn metadata_relay_failed_or_unwound_edits_restore_scope_and_disable_evidence() {
        for unwind in [false, true] {
            let mut k = Kernel::default();
            let old = Arc::new(Mutex::new(Vec::new()));
            if unwind {
                k.context
                    .set_user_notify_callback(Some(Box::new(RelayLog(old.clone()))));
            }
            let work = ProofWork::new(32_000);
            let relay = MetadataRelay::new(work.clone()).unwrap();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Context::with_metadata_relay(&mut k, Some(relay.clone()), |k| {
                    Context::with_committed_notifications(k, |k| {
                        relay_event(&mut k.context, 21);
                        assert!(!unwind, "test atomic edit unwind");
                        Err::<(), _>(SolidError::invalid_input("relay test", "refused edit"))
                    })
                })
            }));
            if unwind {
                assert!(result.is_err());
            } else {
                assert!(matches!(
                    result.unwrap(),
                    Err(SolidError::InvalidInput {
                        operation: "relay test",
                        ..
                    })
                ));
            }
            assert!(k.context.metadata_relay.is_none());
            assert!(old.lock().unwrap().is_empty());
            assert!(work.is_disabled());
            let log = Arc::new(Mutex::new(Vec::new()));
            relay.drain_into(&mut relay_destination(&work, &log));
            assert!(log.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn metadata_relay_exhaustion_keeps_original_callback_and_result() {
        let mut k = Kernel::default();
        let old = Arc::new(Mutex::new(Vec::new()));
        k.context
            .set_user_notify_callback(Some(Box::new(RelayLog(old.clone()))));
        let work = ProofWork::new(1_000);
        let relay = MetadataRelay::new(work.clone()).unwrap();
        let value = Context::with_metadata_relay(&mut k, Some(relay.clone()), |k| {
            relay_event(&mut k.context, 31);
            Ok(7)
        })
        .unwrap();
        assert_eq!(value, 7);
        assert_eq!(*old.lock().unwrap(), vec![ObjectKey(31)]);
        assert!(work.is_disabled());
        assert!(k.context.metadata_relay.is_none());
        let log = Arc::new(Mutex::new(Vec::new()));
        relay.drain_into(&mut relay_destination(&work, &log));
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn metadata_relay_conflict_preserves_outer_stream_and_pending_destination() {
        let mut k = Kernel::default();
        let work = ProofWork::new(32_000);
        let relay = MetadataRelay::new(work.clone()).unwrap();
        let other_work = ProofWork::new(32_000);
        let other = MetadataRelay::new(other_work.clone()).unwrap();
        Context::with_metadata_relay(&mut k, Some(relay.clone()), |k| {
            Context::with_metadata_relay(k, Some(other.clone()), |k| {
                relay_event(&mut k.context, 41);
                Ok(())
            })
        })
        .unwrap();
        assert!(other_work.is_disabled());
        assert!(!work.is_disabled());
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut destination = relay_destination(&work, &log);
        destination.pending_notifications = Some(Vec::new());
        relay.drain_into(&mut destination);
        assert!(log.lock().unwrap().is_empty());
        let events = destination.pending_notifications.take().unwrap();
        assert_eq!(events.len(), 1);
        for event in events {
            destination.deliver_observers(event);
        }
        assert_eq!(*log.lock().unwrap(), vec![ObjectKey(41)]);
    }

    #[test]
    fn metadata_relay_typed_producers_opt_in_without_replacing_old_observer() {
        let mut k = Kernel::default();
        let old = Arc::new(Mutex::new(Vec::new()));
        k.context
            .set_user_notify_callback(Some(Box::new(RelayLog(old.clone()))));
        let work = ProofWork::new(32_000);
        let relay = MetadataRelay::new(work.clone()).unwrap();
        Context::with_metadata_relay(&mut k, Some(relay.clone()), |k| {
            assert!(object::reserve_boundary_source_work(
                Some(&mut k.context),
                2
            ));
            let source = object::ObjectRef {
                type_code: crate::topology::edge::TYPE,
                key: ObjectKey(51),
            };
            object::report_boundary_edit(
                Some(&mut k.context),
                object::BoundaryEdit::Copied {
                    source,
                    target: object::ObjectRef {
                        key: ObjectKey(52),
                        ..source
                    },
                },
            );
            Ok(())
        })
        .unwrap();
        let events = relay.state.lock().unwrap();
        assert_eq!(events.events.len(), 1);
        assert!(
            matches!(&events.events[0], CallbackEvent::BoundaryEdit(edit)
            if matches!(**edit, object::BoundaryEdit::Copied { source, target }
                if source.key == ObjectKey(51) && target.key == ObjectKey(52)))
        );
        assert!(
            old.lock().unwrap().is_empty(),
            "the original observer opted out and must receive no boundary callback"
        );
    }
}
