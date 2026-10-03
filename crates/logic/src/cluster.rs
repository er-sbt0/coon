//! Hierarchical clustering of the call graph by modularity.
//!
//! A whole call graph is unreadable at once — the point of clustering is to give
//! the frontends a coarser thing to draw first, and a way to zoom in. The
//! objective is *cohesion*: a box should hold functions that call each other
//! more than chance would predict given how busy each of them is. That is
//! modularity (Newman and Girvan, 2004), and the optimiser is the Louvain
//! method (Blondel et al., 2008): sweep the nodes, move each into whichever
//! neighbouring community gains the most, repeat until nothing moves, then
//! collapse every community into one node and go again. Each collapse is one
//! rung of the zoom ladder.
//!
//! Why cohesion and not the heaviest edge. This module was heavy-edge matching
//! before — the coarsener METIS shrinks a graph with — and on a real project it
//! grouped by what merges cheaply, which is not what belongs together: 328 of
//! the 370 calls weighed one, so "heaviest" was a tie decided by symbol id;
//! leaves were served first and took the one utility they called, and
//! `log_write`, `mem_free` and `strcmp` then grew a box of every caller they
//! had, cutting straight across the source tree. Of the 33 boxes the map
//! opened on, 26 spanned more than one file, and 13 pairs of boxes called each
//! other both ways where the by-file grouping of the same code has one such
//! pair. Modularity answers both faults at once. A call to a utility that
//! everything calls is *expected* under the null model, so it does not pay for
//! a merge, and a sweep can move a function out of a box it was put in wrongly,
//! where a pair once matched was matched for good.
//!
//! The rungs are a *resolution* ladder. Modularity takes a resolution γ
//! (Reichardt and Bornholdt, 2006) that scales the null model: above one only
//! tightly knit groups pay for themselves, at one it is plain modularity, and
//! below one merging goes on past it. [`coarsen`] runs one pass per entry of
//! [`RESOLUTION_LADDER`], finest first, keeping a rung for every pass that
//! changed something; then passes at one until the optimiser is done; and only
//! then, only while the top is still wider than [`TARGET_TOP_CLUSTERS`], halves
//! γ below one. So every rung is a cohesive grouping at some grain, the last
//! rung at γ = 1 is where the map opens, and the top is a summary a person can
//! take in at a glance, at most a few zoom steps further out.
//!
//! What it does not claim: the resolution limit is real — plain modularity
//! cannot see a module smaller than about √(2m) calls' worth inside a much
//! larger program. Those show at the finer rungs, which is what they are for.
//!
//! It is fully deterministic: nodes are indexed in symbol-id order and swept in
//! that order, candidate communities are tried in ascending id, and a move needs
//! a strictly better gain, so a tie keeps a node where it is. Two runs over the
//! same graph produce byte-identical output, and no seeding or history is
//! needed to get it — which is why there is none.
//!
//! **Isolated functions are left out of the clustering entirely.** A function
//! with no calls in either direction gets no cluster at all, and
//! [`Clustering::cluster_of`] returns `None` for it. This is deliberate: a
//! real project loaded through clangd carries a long tail of symbols that
//! nothing in the indexed sources calls, and giving each one a box of its own
//! would drown the map in hundreds of one-node boxes. A self-recursive function
//! with no other edges counts as isolated too, since self-loops are dropped
//! when the base graph is built, and what is left connects it to nothing.
//!
//! **The quotient of every rung is acyclic.** Modularity alone puts no
//! constraint on direction: a community can sit on both ends of a one-way
//! path, and left to itself the optimiser drew `A -> B` beside `B -> A` over
//! code where nothing calls back — on a real project 19 of the 55 lines on the
//! top rung ran both ways, over a call graph with no cycle in it. So a move is
//! legal only if the community it joins sits on no directed cycle of the
//! quotient afterwards ([`joins_a_cycle`]); a candidate that would close one
//! is skipped and the next best is scored. Every pass starts from a DAG and
//! hands one on, and the singleton fold is held to the same rule. The base
//! graph is made a DAG first by condensing its strongly connected components:
//! mutually recursive functions are one unit from the finest rung up, which
//! is where they belong anyway. On the project above the constraint cost 0.04
//! of modularity and four boxes at the opening rung.
//!
//! **Markov clustering is the second method.** [`ClusterMethod::Markov`] runs
//! van Dongen's MCL (2000) instead of Louvain: a random walk over the calls,
//! alternately *expanded* — the transition matrix squared, so flow follows
//! every two-step path at once — and *inflated* — every entry raised to a
//! power and the column renormalised, so strong currents are fed and weak
//! ones starved — until the flow settles and every function drains into an
//! attractor. Functions sharing an attractor share a box. Closeness is how
//! many short paths join two functions rather than whether any one call does,
//! which is what keeps a single stray call from gluing two groups together;
//! the inflation is the only knob, higher splitting finer, and no count of
//! boxes is asked for. Its ladder is built the same way as modularity's: the
//! opening rung is MCL at [`MARKOV_INFLATION`], and the rungs above it re-run
//! MCL on the quotient with the inflation eased towards one until the top
//! fits. It is held to the acyclicity rule at the one step that admits it —
//! where the settled flow is read off into boxes ([`markov_pass`]) — and the
//! flow itself is computed sparsely, because dense squaring is cubic in the
//! function count.
//!
//! **Leiden is the third method.** [`ClusterMethod::Leiden`] optimises the
//! same modularity as the first, with the Leiden algorithm (Traag, Waltman and
//! van Eck, 2019) in place of Louvain. Louvain can hand back a box that is not
//! connected: a node that was the bridge between two halves of its box moves
//! out, and the halves stay together because nothing re-examines them once
//! the box is collapsed into one node. Leiden puts a *refinement* between the
//! moving and the collapsing: every box is broken back into singletons and
//! rebuilt by merging only nodes that are well connected to the box, into
//! sub-boxes that are well connected to it too, so what gets collapsed is
//! connected by construction. The collapsed nodes then start the next round
//! in the box they came from rather than alone, and the run is over when
//! every box is one node. One run to convergence is one pass of the ladder
//! ([`leiden_pass`]): its internal rounds are scaffolding, not rungs — the
//! unrefined partitions do not nest across them, and the refined ones are not
//! what the method stands behind. The ladder around it is modularity's, pass
//! for pass. The paper picks each refinement merge at random, weighted by
//! gain; this takes the best, which is the zero-temperature limit and keeps
//! the module deterministic, forfeiting only the guarantees that need an
//! unbounded number of iterations. The DAG rule applies at every merge,
//! including the refinement's: a finer partition of an acyclic quotient is
//! not itself acyclic, a chain of three in one box being a two-way cycle once
//! its ends are split from its middle. Refusing a merge never disconnects
//! anything, so the constraint costs modularity, not the guarantee.
//!
//! The implementation works in index space — every hot loop indexes `Vec`s
//! rather than hashing `SymbolId`s — so it stays usable on the ~10k-node graphs
//! a mid-sized C++ project produces.

use model::{CallGraph, SymbolId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Stop coarsening once the coarsest level is this wide or narrower.
///
/// Plain modularity decides how many boxes a program *has*; this decides how
/// few a person can take in at a glance. When the modularity optimum is wider
/// than this, the ladder keeps merging at ever lower resolution until the top
/// fits, and each of those rungs is one more zoom step out from where the map
/// opens. When it is narrower, the constant does nothing: a program with seven
/// modules gets seven boxes, not twelve.
///
/// A pass merges whatever pays at its resolution, so the top lands at or under
/// the target rather than on it. Twelve puts the top at roughly six to twelve
/// boxes, which is a map a person reads rather than counts.
///
/// The default, since [`ClusterSettings::TARGET_TOP`]: a reader who wants the
/// summary tighter or the ladder shorter says so per workspace, and this is
/// where everyone who has not starts.
pub const TARGET_TOP_CLUSTERS: usize = 12;

/// The resolutions the ladder runs down to the chosen one, finest first, as
/// multiples of it.
///
/// γ scales the null model: a function joins a box when the calls between them
/// beat γ times what its busyness alone would predict. At 8 only a function and
/// the helpers it all but owns pay for the merge, and the rung is boxes of two
/// and three; each halving lets the next larger grouping through; 1 is the
/// resolution the user picked — plain modularity unless they moved it — and the
/// rung the map opens on. Halving rather than a finer step because the gain
/// compares a ratio, and a rung a reader can tell from the one below it needs
/// that ratio to move by a factor, not a fraction. A pass that changes nothing
/// adds no rung, so a small program simply has fewer.
///
/// Multiples rather than absolute γs so that [`ClusterSettings::RESOLUTION`]
/// moves the whole ladder rather than only its last rung: the rungs below the
/// opening one are meant to be finer than it, whatever grain it was built at.
pub const RESOLUTION_LADDER: &[f64] = &[8.0, 4.0, 2.0, 1.0];

/// Plain modularity, and where [`ClusterSettings::RESOLUTION`] sits until a
/// user moves it.
pub const DEFAULT_RESOLUTION: f64 = 1.0;

/// How far below plain modularity the ladder will go to reach
/// [`TARGET_TOP_CLUSTERS`].
///
/// A floor rather than a count of halvings, because a graph of disconnected
/// components has nothing any resolution will merge, and the ladder has to stop
/// wide rather than spin.
const MIN_RESOLUTION: f64 = 1.0 / 64.0;

/// A pass that merges nothing ends its phase on its own, so this only bounds
/// the pathological case. Each productive pass removes at least one node, and
/// a graph large enough to need 64 rounds of merging does not exist.
const MAX_ROUNDS: usize = 64;

/// Sweeps within one pass. Every sweep that moves a node raises modularity,
/// which is bounded, so the loop terminates on its own; this guards the
/// floating-point corner where a gain hovers at the tolerance.
const MAX_SWEEPS: usize = 64;

/// A move has to beat staying put by this much. Gains are differences of call
/// counts scaled by degrees over the total, so a real improvement is orders of
/// magnitude above it and a rounding artefact orders below.
const GAIN_EPSILON: f64 = 1e-9;

/// Above this many members, picking a gateway by within-cluster reachability
/// costs a BFS per member. Fall back to out-degree, which is one pass.
const REACHABILITY_MEMBER_LIMIT: usize = 200;

/// The inflation Markov clustering runs at: the power every column of the
/// flow matrix is raised to between expansions.
///
/// MCL's one knob. Higher starves weak flow sooner and the walk settles into
/// more, smaller boxes; lower lets it spread and settles into fewer. Two is
/// van Dongen's default, and the grain the map opens on — and, since
/// [`ClusterSettings::INFLATION`], the grain it opens on for a user who has not
/// said otherwise.
pub const MARKOV_INFLATION: f64 = 2.0;

/// How close to one the inflation is eased, above the opening rung, to reach
/// [`TARGET_TOP_CLUSTERS`].
///
/// At one exactly MCL is a plain random walk, which settles into one box per
/// connected component; just above it a few attractors survive. Each easing
/// halves the distance to one, and this is the floor, for the same reason
/// [`MIN_RESOLUTION`] is one: a graph of disconnected components has nothing
/// any inflation will merge, and the ladder has to stop wide rather than spin.
const MIN_INFLATION: f64 = 1.0 + 1.0 / 32.0;

/// Expansion–inflation rounds before the flow is read off whether it has
/// settled or not. On a call graph it settles in a few dozen; the cap bounds
/// the graphs it oscillates on, and what it reads off then is still a
/// partition.
const MARKOV_MAX_ITERATIONS: usize = 100;

/// The flow has settled when no entry moved by more than this in a round.
const MARKOV_CONVERGENCE: f64 = 1e-6;

/// An entry below this share of its column is dropped after every round.
///
/// This is what keeps the matrix sparse: the entries pruned are the ones
/// inflation was about to starve anyway, and without it every column fills in
/// after a few expansions and a round costs the cube of the function count.
const MARKOV_PRUNE_THRESHOLD: f64 = 1e-4;

/// At most this many entries survive in a column, the heaviest kept, which
/// bounds one round at `n · K²` however dense the graph is.
const MARKOV_PRUNE_KEEP: usize = 100;

/// Which algorithm builds the coarsening ladder.
///
/// A clustering is a claim about a program's shape, and every method makes a
/// different one — modularity answers "who calls each other more than chance
/// would", flow answers "where does a walk along the calls end up". Which
/// claim is on screen is the user's to pick, so it is a value that travels
/// with the workspace and the snapshot rather than a constant compiled into
/// one call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ClusterMethod {
    /// Modularity optimised by the Louvain method, described at the top of
    /// this module.
    #[default]
    Modularity,
    /// Modularity again, optimised by the Leiden algorithm, which keeps every
    /// box connected. Described at the top of this module.
    Leiden,
    /// Markov clustering — flow simulation with inflation — also described
    /// at the top of this module.
    Markov,
}

impl ClusterMethod {
    /// Every method, in the order a picker should list them.
    pub const ALL: &'static [ClusterMethod] = &[
        ClusterMethod::Modularity,
        ClusterMethod::Leiden,
        ClusterMethod::Markov,
    ];

    /// What to write in the picker.
    pub fn label(self) -> &'static str {
        match self {
            ClusterMethod::Modularity => "Modularity",
            ClusterMethod::Leiden => "Leiden",
            ClusterMethod::Markov => "Markov flow",
        }
    }

    /// One line on what the method looks for, for the picker's second row.
    pub fn description(self) -> &'static str {
        match self {
            ClusterMethod::Modularity => {
                "Groups functions that call each other more than chance would"
            }
            ClusterMethod::Leiden => "Modularity again, with every box guaranteed to hang together",
            ClusterMethod::Markov => {
                "Groups functions a random walk along the calls drains into together"
            }
        }
    }

    /// The one knob this method turns, and the range it turns over.
    ///
    /// Every method has exactly one, because every method has exactly one
    /// thing to say about grain: modularity and Leiden climb the same
    /// objective and so share γ, and flow has its inflation. A picker draws
    /// whichever this names and never needs to know which method it belongs
    /// to — which is what keeps a method added here reachable without touching
    /// a frontend, the same bargain [`ClusterMethod::label`] makes.
    pub fn grain_spec(self) -> SettingSpec {
        match self {
            ClusterMethod::Modularity | ClusterMethod::Leiden => ClusterSettings::RESOLUTION,
            ClusterMethod::Markov => ClusterSettings::INFLATION,
        }
    }
}

/// A knob's name, range and default — everything a picker needs to draw it.
///
/// It ships in the snapshot beside the method that reads it, for the reason
/// [`ClusterMethod::label`] does: a range written into a frontend instead is a
/// range that goes on meaning what it meant the day it was copied, and a knob
/// retuned here then reads one thing in the client and does another.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SettingSpec {
    /// Which field of [`ClusterSettings`] a control moves.
    pub field: SettingField,
    pub label: &'static str,
    /// One line on what turning it up does.
    pub description: &'static str,
    pub min: f64,
    pub max: f64,
    /// The grain of the control *and* of the value: everything arriving is
    /// rounded to a multiple of this. Two users who dragged a slider to the
    /// same place are asking the same question, and a request that differs
    /// only in the last bits of a float is a cache miss and a second run of
    /// the whole coarsening.
    pub step: f64,
    pub default: f64,
}

impl SettingSpec {
    /// The nearest legal value to `value`: clamped to the range, then rounded
    /// to a multiple of [`SettingSpec::step`].
    ///
    /// Total, because the only callers are a wire hop and a frontend: a
    /// nonsense value is answered with the default rather than refused, since
    /// there is no one on the other end of a refusal who could do better.
    pub fn sanitise(&self, value: f64) -> f64 {
        if !value.is_finite() {
            return self.default;
        }
        let clamped = value.clamp(self.min, self.max);
        (clamped / self.step).round() * self.step
    }
}

/// Which setting a [`SettingSpec`] describes, and so which field of
/// [`ClusterSettings`] a control writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingField {
    Resolution,
    Inflation,
    TargetTop,
}

/// What a clustering was asked for: the method, and the knobs that method
/// reads.
///
/// One value rather than a method beside a loose bag of parameters, because
/// they are one question. A workspace holds one, a cache entry is keyed by
/// one, a worker computes one — and every part of it changes the answer, so
/// anything keyed on the method alone would hand back a map built at another
/// inflation as though it were this one.
///
/// Every method's knob is carried, not just the selected one's. A user who
/// tunes the flow, looks at modularity and comes back has not thrown the
/// tuning away, and each method keeps its own grain rather than sharing one
/// number that means γ to two of them and an inflation to the third.
///
/// Stored in hundredths so the whole thing is `Eq` and `Hash`: this is a cache
/// key, and a key made of floats is one that two equal-looking requests can
/// miss each other on. The API and the wire speak plain numbers —
/// [`WireSettings`] is the hop between, and it sanitises on the way in, so a
/// value off the step or out of range cannot reach the ladder from a frontend
/// at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "WireSettings", into = "WireSettings")]
pub struct ClusterSettings {
    method: ClusterMethod,
    /// γ, in hundredths.
    resolution: u16,
    /// MCL inflation, in hundredths.
    inflation: u16,
    /// The widest the coarsest rung may be, in boxes.
    target_top: u16,
}

impl ClusterSettings {
    /// γ, the resolution the modularity methods build their ladder around.
    ///
    /// The ladder is [`RESOLUTION_LADDER`] *times this*, so moving it moves
    /// every rung together rather than only the one the map opens on: at γ = 2
    /// the opening rung is what γ = 2 pays for and the rungs below it are
    /// finer still. One is plain modularity and the default, and the range
    /// stops where the objective stops saying anything a reader can use — a
    /// quarter merges nearly everything, four merges little but what one
    /// function all but owns.
    pub const RESOLUTION: SettingSpec = SettingSpec {
        field: SettingField::Resolution,
        label: "Resolution γ",
        description: "Higher only pays for tighter groups, so the map opens on more, smaller boxes",
        min: 0.25,
        max: 4.0,
        step: 0.25,
        default: DEFAULT_RESOLUTION,
    };

    /// The power MCL raises every column to between expansions.
    ///
    /// Bounded below well above one because at one exactly MCL is a plain
    /// random walk that settles into one box per connected component, which is
    /// a map of nothing; the ladder still eases below this floor on its own to
    /// reach the target, one rung at a time, where a user cannot.
    pub const INFLATION: SettingSpec = SettingSpec {
        field: SettingField::Inflation,
        label: "Inflation",
        description:
            "Higher starves weak flow sooner, so the walk settles into more, smaller boxes",
        min: 1.2,
        max: 4.0,
        step: 0.1,
        default: MARKOV_INFLATION,
    };

    /// How wide the coarsest rung may be.
    ///
    /// It does not trim a grouping and never touches the rung the map opens
    /// on: it decides only how far past its own grain the ladder keeps merging,
    /// so lowering it adds zoom steps and raising it takes them away. See
    /// [`TARGET_TOP_CLUSTERS`].
    pub const TARGET_TOP: SettingSpec = SettingSpec {
        field: SettingField::TargetTop,
        label: "Coarsest rung",
        description: "How few boxes the ladder keeps merging towards — it only ever adds rungs to zoom out to",
        min: 4.0,
        max: 40.0,
        step: 1.0,
        default: TARGET_TOP_CLUSTERS as f64,
    };

    /// `method` at every knob's default.
    pub fn new(method: ClusterMethod) -> Self {
        Self {
            method,
            resolution: hundredths(Self::RESOLUTION.default),
            inflation: hundredths(Self::INFLATION.default),
            target_top: Self::TARGET_TOP.default as u16,
        }
    }

    pub fn method(self) -> ClusterMethod {
        self.method
    }

    /// The same knobs, under a different method.
    pub fn with_method(self, method: ClusterMethod) -> Self {
        Self { method, ..self }
    }

    pub fn resolution(self) -> f64 {
        f64::from(self.resolution) / 100.0
    }

    pub fn inflation(self) -> f64 {
        f64::from(self.inflation) / 100.0
    }

    pub fn target_top(self) -> usize {
        usize::from(self.target_top)
    }

    /// The knob the chosen method actually reads — γ or the inflation — as
    /// [`ClusterMethod::grain_spec`] names it.
    pub fn grain(self) -> f64 {
        match self.method.grain_spec().field {
            SettingField::Inflation => self.inflation(),
            _ => self.resolution(),
        }
    }

    /// Move the chosen method's grain, sanitised. The other method's is left
    /// where it was.
    pub fn with_grain(self, value: f64) -> Self {
        let spec = self.method.grain_spec();
        let value = hundredths(spec.sanitise(value));
        match spec.field {
            SettingField::Inflation => Self {
                inflation: value,
                ..self
            },
            _ => Self {
                resolution: value,
                ..self
            },
        }
    }

    /// Move the width the ladder coarsens towards, sanitised.
    pub fn with_target_top(self, boxes: usize) -> Self {
        Self {
            target_top: Self::TARGET_TOP.sanitise(boxes as f64) as u16,
            ..self
        }
    }
}

impl Default for ClusterSettings {
    fn default() -> Self {
        Self::new(ClusterMethod::default())
    }
}

/// So that a caller with nothing to say about the knobs can pass a method
/// where settings are wanted — which is most of them, and every one that
/// existed before the knobs did.
impl From<ClusterMethod> for ClusterSettings {
    fn from(method: ClusterMethod) -> Self {
        Self::new(method)
    }
}

/// [`ClusterSettings`] as a frontend reads and writes them: plain numbers.
///
/// Public because it is the shape on the wire, and a protocol is worth being
/// able to name. Everything arriving this way is sanitised by the conversion
/// below, and a field left out is the default — a frontend that knows about
/// the method and nothing else still sends a settings object that works.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct WireSettings {
    pub method: ClusterMethod,
    pub resolution: f64,
    pub inflation: f64,
    pub target_top: usize,
}

impl Default for WireSettings {
    fn default() -> Self {
        ClusterSettings::default().into()
    }
}

impl From<WireSettings> for ClusterSettings {
    fn from(wire: WireSettings) -> Self {
        Self {
            method: wire.method,
            resolution: hundredths(Self::RESOLUTION.sanitise(wire.resolution)),
            inflation: hundredths(Self::INFLATION.sanitise(wire.inflation)),
            target_top: Self::TARGET_TOP.sanitise(wire.target_top as f64) as u16,
        }
    }
}

impl From<ClusterSettings> for WireSettings {
    fn from(settings: ClusterSettings) -> Self {
        Self {
            method: settings.method(),
            resolution: settings.resolution(),
            inflation: settings.inflation(),
            target_top: settings.target_top(),
        }
    }
}

/// Every setting is held in hundredths of its unit; both knobs step in
/// hundredths or coarser, so nothing is lost in the rounding.
fn hundredths(value: f64) -> u16 {
    (value * 100.0).round().max(0.0) as u16
}
/// Index handle for a cluster within one [`Clustering`], valid until the next
/// recompute and no longer.
///
/// Ids are assigned sequentially when the ladder is flattened, in the order the
/// clusters at a level sort by their lowest member, so they are *not* names for
/// a group: the same set of functions routinely comes back under a different id
/// once the graph grows, because one new symbol sorting ahead of the others
/// renumbers everything behind it.
///
/// So an id is for indexing the clustering you are holding right now. Anything
/// that outlives it — workspace state, a snapshot the frontend keeps, a command
/// arriving back over the wire — must name clusters with a [`ClusterKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClusterId(pub u32);

/// A name for a cluster that survives a recompute.
///
/// The clusters at one level partition the clustered symbols — every clustered
/// symbol is in exactly one of them — so any member chosen by a rule that looks
/// only at the member set picks out its cluster uniquely within that level. The
/// lowest member by uuid is the cheapest such rule, and it is already the one
/// `build_clustering` uses to order clusters within a level, so the anchor is
/// simply the first entry of [`Cluster::members`]. A level plus that anchor is
/// therefore something a frontend can hold across a recompute, and it goes on
/// naming the same group for as long as that group keeps its lowest member.
///
/// It is a durable handle rather than an identity, and the way it slips is
/// worth being plain about: a cluster whose anchor is merged into a different
/// pair comes back under a key of its own, and the old key now names whichever
/// cluster took that member in. A key someone was holding then stops resolving
/// to what they opened. That is a re-collapse, not corruption: nothing is lost
/// that the next click does not restore, which is exactly what a [`ClusterId`]
/// — stale after *every* recompute, and silently pointing at some unrelated
/// group — cannot promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClusterKey {
    pub level: usize,
    /// The cluster's lowest member, ordered by uuid bytes — the same ordering
    /// every other ordering in this module descends from.
    pub anchor: SymbolId,
}

/// One cluster at one level of the ladder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cluster {
    pub id: ClusterId,
    /// The handle to use for anything that outlives this `Clustering`.
    pub key: ClusterKey,
    pub level: usize,
    pub parent: Option<ClusterId>,
    /// The one or two clusters one level finer that merged into this one; empty
    /// at level 0.
    pub children: Vec<ClusterId>,
    /// Every leaf function under this cluster, transitively, ordered by symbol
    /// id so two runs produce byte-identical output.
    pub members: Vec<SymbolId>,
    pub entry_point: Option<SymbolId>,
    pub label: String,
}

/// A coarsening ladder over a [`CallGraph`]'s connected functions.
///
/// Level 0 is the finest — one round of pair merges — and the last level the
/// coarsest. Every level partitions the same set of symbols; a level is not a
/// subset of the map above it but the same map drawn at a finer grain, which is
/// what lets a frontend present the ladder as a zoom rather than as a tree to
/// open.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Clustering {
    /// Indexed by `ClusterId.0`.
    clusters: Vec<Cluster>,
    /// `levels[l]` holds the ids at level `l`, finest first.
    levels: Vec<Vec<ClusterId>>,
    /// Level-0 lookup; isolated symbols are absent by design.
    index: HashMap<SymbolId, ClusterId>,
    /// `anchors[l]` resolves a level-`l` [`ClusterKey`]'s anchor to its cluster,
    /// so [`Clustering::by_key`] is a lookup rather than a scan over every
    /// cluster at the level.
    anchors: Vec<HashMap<SymbolId, ClusterId>>,
    /// The rung a map should open on — the last one built at plain
    /// modularity. Not necessarily the top, and never past it.
    opening: usize,
    /// What it was built from — method and knobs — so a holder can tell
    /// whether the clustering it has is the one that was asked for.
    settings: ClusterSettings,
}

impl Clustering {
    /// Cluster the whole graph.
    ///
    /// Takes anything that is settings or can become them, so a caller with
    /// nothing to say about the knobs passes a [`ClusterMethod`] and gets that
    /// method at its defaults.
    pub fn compute(graph: &CallGraph, settings: impl Into<ClusterSettings>) -> Self {
        Self::compute_scoped(graph, None, settings.into())
    }

    /// Coarsen one corner of the graph: only the functions in `scope`, with
    /// every call that leaves it ignored.
    ///
    /// This is not a filter applied to the whole-program ladder, and could not
    /// be. What a merge is worth is judged against the null model of the graph
    /// being clustered — how busy each function is out of *all* the calls
    /// present — and by how many passes it takes to reach the target width,
    /// both of which change completely when the graph is one function's
    /// neighbourhood rather than the program. Run over a neighbourhood, the
    /// boxes that come back divide that neighbourhood, where the whole-program
    /// run would have merged the lot into a single box several rounds down.
    /// That is exactly the question a user asks by pointing at a function, so
    /// it gets its own run rather than a view onto the global one.
    ///
    /// Isolation is judged inside the scope too: a function whose only calls
    /// leave it has no edges here, and is dropped like any other isolated
    /// symbol. Entry points and labels still read the full graph, because being
    /// called from outside the scope is what makes a member a front door.
    pub fn compute_within(
        graph: &CallGraph,
        scope: &HashSet<SymbolId>,
        settings: impl Into<ClusterSettings>,
    ) -> Self {
        Self::compute_scoped(graph, Some(scope), settings.into())
    }

    /// The method this ladder was built with.
    pub fn method(&self) -> ClusterMethod {
        self.settings.method()
    }

    /// Everything this ladder was built from, knobs included.
    pub fn settings(&self) -> ClusterSettings {
        self.settings
    }

    fn compute_scoped(
        graph: &CallGraph,
        scope: Option<&HashSet<SymbolId>>,
        settings: ClusterSettings,
    ) -> Self {
        let base = build_base_graph(graph, scope);
        if base.symbols.is_empty() {
            return Self {
                settings,
                ..Self::default()
            };
        }

        let mut ladder = coarsen(base.graph, base.sizes, settings);
        // The ladder's first rung maps condensed nodes; `build_clustering`
        // reads a map from symbols, so the condensation is composed in.
        let first: Vec<usize> = base
            .units
            .iter()
            .map(|&unit| ladder.levels[0][unit])
            .collect();
        ladder.levels[0] = first;
        let opening = ladder.opening();
        let mut clustering = build_clustering(graph, &base.symbols, &ladder.levels);
        clustering.opening = opening;
        clustering.settings = settings;
        clustering
    }

    pub fn is_empty(&self) -> bool {
        self.clusters.is_empty()
    }

    /// How many rungs the ladder has. The zoom control's range.
    pub fn level_count(&self) -> usize {
        self.levels.len()
    }

    /// The coarsest level — the boxes a frontend should draw first.
    pub fn top_level(&self) -> &[ClusterId] {
        self.levels.last().map(Vec::as_slice).unwrap_or(&[])
    }

    /// The index of the coarsest level, or `None` when nothing is clustered.
    pub fn top_level_index(&self) -> Option<usize> {
        self.levels.len().checked_sub(1)
    }

    /// The rung a map should open on.
    ///
    /// The last rung built at the method's own grain — plain modularity,
    /// γ = 1, or MCL at [`MARKOV_INFLATION`]: the grouping the method itself
    /// stands behind. Coarser rungs exist only when that one was wider than
    /// [`TARGET_TOP_CLUSTERS`], and they are where a user zooms out *to*, not
    /// where they should land. See [`Ladder::opening`].
    pub fn opening_level(&self) -> usize {
        self.opening.min(self.levels.len().saturating_sub(1))
    }

    /// The clusters at `level`, or an empty slice if the level does not exist.
    pub fn level(&self, level: usize) -> &[ClusterId] {
        self.levels.get(level).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn cluster(&self, id: ClusterId) -> Option<&Cluster> {
        self.clusters.get(id.0 as usize)
    }

    /// The cluster named by `key`, or `None` when nothing at that level is
    /// anchored on that symbol — a level this clustering does not have, or a
    /// key from before a recompute that merged its anchor elsewhere.
    pub fn by_key(&self, key: ClusterKey) -> Option<&Cluster> {
        self.anchors
            .get(key.level)
            .and_then(|level| level.get(&key.anchor))
            .and_then(|id| self.cluster(*id))
    }

    /// The finest (level 0) cluster holding `symbol`, or `None` if the symbol is
    /// isolated or absent.
    pub fn cluster_of(&self, symbol: &SymbolId) -> Option<ClusterId> {
        self.index.get(symbol).copied()
    }

    /// The cluster at `level` holding `symbol`, or `None` if the symbol is
    /// isolated, absent, or the level does not exist.
    ///
    /// Every level partitions the same symbols, so this always answers for a
    /// clustered symbol — which is what a flat map at one rung needs in order
    /// to say which box a function is in without walking the ladder itself.
    pub fn cluster_of_at(&self, symbol: &SymbolId, level: usize) -> Option<ClusterId> {
        let mut current = self.cluster_of(symbol)?;
        while let Some(cluster) = self.cluster(current) {
            if cluster.level >= level {
                return (cluster.level == level).then_some(current);
            }
            current = cluster.parent?;
        }
        None
    }

    /// The chain of clusters containing `symbol`, finest first, coarsest last.
    pub fn ancestry(&self, symbol: &SymbolId) -> Vec<ClusterId> {
        let mut chain = Vec::new();
        let mut current = self.cluster_of(symbol);
        while let Some(id) = current {
            chain.push(id);
            current = self.cluster(id).and_then(|c| c.parent);
        }
        chain
    }

    pub fn clusters(&self) -> impl Iterator<Item = &Cluster> + '_ {
        self.clusters.iter()
    }
}

// ---------------------------------------------------------------------------
// Weighted graph in index space
// ---------------------------------------------------------------------------

struct WeightedGraph {
    /// Neighbours and weights, sorted by index and never containing the node
    /// itself.
    adj: Vec<Vec<(usize, f64)>>,
    /// Weight of each node's self-loop — the calls that merging a pair turned
    /// inward. Carried so a coarser round can tell a heavy internal edge from a
    /// heavy external one, and so nothing about the original graph is lost as
    /// the ladder climbs.
    self_loops: Vec<f64>,
    /// Successors of each node, sorted and deduplicated: the direction `adj`
    /// dropped, kept so a move can be tested for closing a cycle in the
    /// quotient. Weights are not carried — the test is reachability, and the
    /// objective reads `adj`.
    out: Vec<Vec<usize>>,
}

impl WeightedGraph {
    fn new(
        adj_maps: Vec<HashMap<usize, f64>>,
        self_loops: Vec<f64>,
        mut out: Vec<Vec<usize>>,
    ) -> Self {
        let mut adj: Vec<Vec<(usize, f64)>> = adj_maps
            .into_iter()
            .map(|m| m.into_iter().collect::<Vec<_>>())
            .collect();
        // The singleton fold breaks weight ties on the lower partner index,
        // which it gets for free by scanning in index order — so this sort is
        // not cosmetic, it *is* the tie-break.
        for neighbours in &mut adj {
            neighbours.sort_unstable_by_key(|(index, _)| *index);
        }
        for successors in &mut out {
            successors.sort_unstable();
            successors.dedup();
        }

        Self {
            adj,
            self_loops,
            out,
        }
    }

    fn node_count(&self) -> usize {
        self.adj.len()
    }
}

/// The graph the coarsening runs on: the connected symbols, condensed so that
/// it is a DAG.
struct Base {
    /// Every connected symbol, in id order — the meaning of every index.
    symbols: Vec<SymbolId>,
    /// The condensed node each symbol is, indexed like `symbols`.
    units: Vec<usize>,
    /// How many symbols each node of `graph` holds.
    sizes: Vec<usize>,
    graph: WeightedGraph,
}

/// Build the weighted graph the coarsening runs on, plus the symbol list that
/// gives every index its meaning.
///
/// `CallGraph::callee_ids` returns one entry per distinct *call site*, not per
/// distinct callee — the graph deduplicates on (caller, callee, location). So
/// the duplicates in that slice are exactly the number of times the caller calls
/// the callee, and counting them is how we get an edge weight out of an API
/// that stores none: `CallGraph::edges` exists but reports the same call sites
/// one at a time and aggregates nothing either, and walking per symbol is what
/// makes `scope` below cost the scope's calls rather than the project's. Those
/// counts are the whole input to the objective: two functions that call each
/// other twenty times are twenty times the reason to put them in one box.
///
/// The objective reads the calls undirected — a call is a reason to put two
/// functions in one box whichever way it runs — and the direction is kept
/// beside them in `out`, for the cycle test alone. The graph is then condensed:
/// every strongly connected component becomes one node, so what the coarsening
/// starts from is a DAG and [`joins_a_cycle`] has an invariant to keep rather
/// than one to hope for. Self-loops are dropped before that, so plain
/// recursion is not a component of its own.
///
/// `scope`, when given, is the set of functions to consider; everything else is
/// left out, and so are the calls that would have reached it. Restricting the
/// symbol list is enough to restrict the edges too, because a call whose other
/// end has no index is skipped exactly as a call to an unknown symbol is.
fn build_base_graph(graph: &CallGraph, scope: Option<&HashSet<SymbolId>>) -> Base {
    let mut all: Vec<SymbolId> = graph
        .function_ids()
        .filter(|id| scope.is_none_or(|s| s.contains(id)))
        .copied()
        .collect();
    // Every downstream ordering — the visit order, member lists, cluster ids —
    // descends from this sort, which is what makes two runs byte-identical.
    all.sort_unstable_by_key(|s| s.0);

    let position: HashMap<SymbolId, usize> = all.iter().enumerate().map(|(i, s)| (*s, i)).collect();

    let mut weights: Vec<HashMap<usize, f64>> = vec![HashMap::new(); all.len()];
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); all.len()];
    for (i, symbol) in all.iter().enumerate() {
        for callee in graph.callee_ids(symbol) {
            let Some(&j) = position.get(callee) else {
                continue;
            };
            // A function cannot be merged with itself, so recursion is weight
            // with nowhere to go.
            if i == j {
                continue;
            }
            *weights[i].entry(j).or_insert(0.0) += 1.0;
            *weights[j].entry(i).or_insert(0.0) += 1.0;
            out[i].push(j);
        }
    }

    // Drop the isolated symbols and compact the index space around what is left.
    let kept: Vec<usize> = (0..all.len()).filter(|i| !weights[*i].is_empty()).collect();
    let mut compacted = vec![usize::MAX; all.len()];
    for (new, &old) in kept.iter().enumerate() {
        compacted[old] = new;
    }

    let symbols: Vec<SymbolId> = kept.iter().map(|&i| all[i]).collect();
    let adj_maps: Vec<HashMap<usize, f64>> = kept
        .iter()
        .map(|&old| {
            weights[old]
                .iter()
                .map(|(&j, &w)| (compacted[j], w))
                .collect()
        })
        .collect();
    let out: Vec<Vec<usize>> = kept
        .iter()
        .map(|&old| out[old].iter().map(|&j| compacted[j]).collect())
        .collect();
    let self_loops = vec![0.0; symbols.len()];
    let graph = WeightedGraph::new(adj_maps, self_loops, out);

    let (units, count) = condense(&graph.out);
    let sizes = merged_sizes(&vec![1; symbols.len()], &units, count);
    let graph = if count == symbols.len() {
        graph
    } else {
        aggregate(&graph, &units, count)
    };

    Base {
        symbols,
        units,
        sizes,
        graph,
    }
}

/// The strongly connected components of a graph given as successor lists,
/// numbered in order of lowest member so that the units come out in the same
/// order as the nodes. Returns the component of each node and the count.
///
/// Tarjan's algorithm with an explicit stack: a call chain a few thousand
/// functions deep is an ordinary thing in a real project, and a recursive walk
/// over it would overflow the thread's stack.
fn condense(out: &[Vec<usize>]) -> (Vec<usize>, usize) {
    let n = out.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut component = vec![usize::MAX; n];
    let mut count = 0usize;
    let mut next = 0usize;
    // The call stack: a node and how many of its successors it has visited.
    let mut work: Vec<(usize, usize)> = Vec::new();

    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        index[start] = next;
        low[start] = next;
        next += 1;
        stack.push(start);
        on_stack[start] = true;
        work.push((start, 0));

        while let Some(&mut (node, ref mut position)) = work.last_mut() {
            if *position < out[node].len() {
                let successor = out[node][*position];
                *position += 1;
                if index[successor] == usize::MAX {
                    index[successor] = next;
                    low[successor] = next;
                    next += 1;
                    stack.push(successor);
                    on_stack[successor] = true;
                    work.push((successor, 0));
                } else if on_stack[successor] {
                    low[node] = low[node].min(index[successor]);
                }
                continue;
            }

            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == index[node] {
                loop {
                    let member = stack
                        .pop()
                        .expect("the root of a component is on the stack");
                    on_stack[member] = false;
                    component[member] = count;
                    if member == node {
                        break;
                    }
                }
                count += 1;
            }
        }
    }

    renumber(&component)
}

// ---------------------------------------------------------------------------
// Modularity
// ---------------------------------------------------------------------------

/// Run passes of `method` until the top is at most [`TARGET_TOP_CLUSTERS`]
/// wide.
///
/// The return is the ladder in the shape [`build_clustering`] reads: entry 0
/// maps a base node to its level-0 cluster, and entry `l` maps a level-`l - 1`
/// cluster to its level-`l` one. So the number of entries is the number of
/// productive passes, and every entry is one rung of zoom. Which passes, in
/// which regimes, is the method's — [`Coarsening::by_modularity`] and
/// [`Coarsening::by_flow`] — and everything around them is shared.
///
/// A graph already at or below the target still gets a rung, made of
/// singletons. A map has to have something to draw, and "every function is its
/// own box" is the honest answer for a graph that small — grouping it further
/// would merge a program small enough to read whole.
fn coarsen(base: WeightedGraph, sizes: Vec<usize>, settings: ClusterSettings) -> Ladder {
    let mut run = Coarsening {
        levels: Vec::new(),
        sizes,
        current: base,
        opening: 0,
        settings,
    };

    if run.current.node_count() > settings.target_top() {
        match settings.method() {
            ClusterMethod::Modularity => run.by_modularity(louvain_pass),
            ClusterMethod::Leiden => run.by_modularity(leiden_pass),
            ClusterMethod::Markov => run.by_flow(),
        }
    }

    if run.levels.is_empty() {
        // A graph small enough to read whole, or one nothing merges. Every
        // function is its own box, and there is nothing to fold: they are
        // *all* singletons, so the pass below would merge the program into one.
        return Ladder {
            levels: vec![(0..run.current.node_count()).collect()],
            opening: 0,
        };
    }

    if let Some((fold, count)) = fold_singletons(&run.current, &run.sizes) {
        // Composed into the last round rather than added as a rung of its own:
        // it is a repair to the top of the ladder, not a step of the zoom, and
        // a rung that differs from the one below it by one box is a rung the
        // user would step through for nothing.
        let last = run.levels.last_mut().expect("the ladder is not empty here");
        for group in last.iter_mut() {
            *group = fold[*group];
        }
        run.current = aggregate(&run.current, &fold, count);
    }

    Ladder {
        levels: run.levels,
        opening: run.opening,
    }
}

/// The ladder as it is being built: the quotient so far and what each of its
/// nodes stands for.
struct Coarsening {
    levels: Vec<Vec<usize>>,
    /// How many base functions each node of `current` holds.
    sizes: Vec<usize>,
    current: WeightedGraph,
    /// The last rung built at the method's own grain.
    opening: usize,
    /// The method and the knobs this ladder is being built to.
    settings: ClusterSettings,
}

impl Coarsening {
    /// The Louvain ladder, in three phases, each a resolution regime:
    ///
    /// 1. One pass per entry of [`RESOLUTION_LADDER`], as a multiple of the
    ///    chosen γ, finest first. A pass that merges nothing adds no rung.
    /// 2. Passes at the chosen γ until one merges nothing — the standard
    ///    Louvain run to its optimum. The last rung this leaves is the opening
    ///    rung.
    /// 3. Only while the top is still wider than the target: halve γ and pass
    ///    again, down to [`MIN_RESOLUTION`].
    ///
    /// The chosen γ is [`ClusterSettings::RESOLUTION`], one unless the user
    /// moved it, and it scales the whole ladder rather than the opening rung
    /// alone — the rungs under the opening one are the ones meant to be finer
    /// than what the user asked for, and they stay finer wherever that is.
    /// The floor below is absolute: it exists because a graph of disconnected
    /// components has nothing any resolution merges, which is as true at γ = 4
    /// as at γ = 1.
    ///
    /// `optimise` is the pass: [`louvain_pass`] or [`leiden_pass`]. Both
    /// climb the same objective, so the regimes are the same and only the
    /// climb differs.
    fn by_modularity(&mut self, optimise: fn(&WeightedGraph, f64) -> (Vec<usize>, usize)) {
        let chosen = self.settings.resolution();
        let target = self.settings.target_top();
        for &step in RESOLUTION_LADDER {
            self.pass(step >= 1.0, |graph| optimise(graph, chosen * step));
        }
        while self.pass(true, |graph| optimise(graph, chosen)) {}
        let mut resolution = chosen / 2.0;
        while self.current.node_count() > target && resolution >= MIN_RESOLUTION {
            self.pass(false, |graph| optimise(graph, resolution));
            resolution /= 2.0;
        }
    }

    /// The Markov ladder, in two phases:
    ///
    /// 1. One pass of MCL at the chosen inflation ([`ClusterSettings::INFLATION`],
    ///    [`MARKOV_INFLATION`] unless the user moved it) over the base graph. That
    ///    is the grouping the method stands behind and the rung the map opens
    ///    on. One pass, not passes until nothing merges: MCL is not an
    ///    optimiser climbing towards an optimum, a run *is* the answer, and a
    ///    run over the quotient is a different, coarser question.
    /// 2. Only while the top is still wider than the target: pass again over
    ///    the quotient, at the same inflation for as long as that merges
    ///    anything, then with the inflation eased halfway to one each time it
    ///    stops, down to [`MIN_INFLATION`]. Easing is the analogue of lowering
    ///    γ — a weaker inflation starves less, so the flow spreads further
    ///    before it settles and fewer attractors survive.
    ///
    /// There are no rungs finer than the opening. MCL at a higher inflation
    /// on the base graph is not a refinement of MCL at a lower one — the two
    /// partitions need not nest — and a ladder is a chain of refinements.
    fn by_flow(&mut self) {
        let chosen = self.settings.inflation();
        let target = self.settings.target_top();
        self.pass(true, |graph| markov_pass(graph, chosen));
        let mut inflation = chosen;
        while self.current.node_count() > target && inflation >= MIN_INFLATION {
            if !self.pass(false, |graph| markov_pass(graph, inflation)) {
                inflation = 1.0 + (inflation - 1.0) / 2.0;
            }
        }
    }

    /// One pass of `partition` over the quotient so far, kept as a rung if it
    /// merged anything, and marked as the opening rung if it did and `opens`
    /// says the pass ran at the method's own grain. Returns whether it merged.
    fn pass(
        &mut self,
        opens: bool,
        partition: impl FnOnce(&WeightedGraph) -> (Vec<usize>, usize),
    ) -> bool {
        if self.levels.len() >= MAX_ROUNDS {
            return false;
        }
        let (groups, count) = partition(&self.current);
        if count == self.current.node_count() {
            return false;
        }
        self.sizes = merged_sizes(&self.sizes, &groups, count);
        self.current = aggregate(&self.current, &groups, count);
        self.levels.push(groups);
        if opens {
            self.opening = self.levels.len() - 1;
        }
        true
    }
}

/// The coarsening, plus where it should open.
struct Ladder {
    /// Entry 0 maps a base node to its level-0 cluster, and entry `l` maps a
    /// level-`l - 1` cluster to its level-`l` one.
    levels: Vec<Vec<usize>>,
    /// The last rung built at the method's own grain.
    opening: usize,
}

impl Ladder {
    /// The rung a map should open on.
    ///
    /// How far the ladder goes and where it opens are separate questions. The
    /// top is bounded by [`TARGET_TOP_CLUSTERS`] so a user has somewhere to zoom
    /// out to; the opening rung is the last one the method built at its own
    /// grain — plain modularity, or MCL at [`MARKOV_INFLATION`] — because that
    /// is the grouping the method stands behind. The rungs above it merge
    /// modules that did not want to merge, to fit a count, and the rungs below
    /// it, where there are any, are the same modules cut finer, for looking
    /// inside.
    fn opening(&self) -> usize {
        self.opening.min(self.levels.len().saturating_sub(1))
    }
}

/// One pass of the Louvain method at `resolution`: start every node in a
/// community of its own, sweep the nodes in index order moving each to the
/// neighbouring community that gains the most modularity, and repeat until a
/// sweep moves nothing.
///
/// Returns the assignment of nodes to communities, renumbered so they are
/// `0..count` in order of lowest member.
///
/// The move itself is [`Mover::place`]; this is the sweep that drives it.
fn louvain_pass(graph: &WeightedGraph, resolution: f64) -> (Vec<usize>, usize) {
    let n = graph.node_count();
    if total_degree(graph) <= 0.0 {
        return renumber(&(0..n).collect::<Vec<_>>());
    }
    let mut mover = Mover::new(graph, resolution, (0..n).collect(), n);
    mover.sweep();
    renumber(&mover.community)
}

/// Every node's degree. A self-loop counts twice, as in the modularity
/// definition, which is what keeps the degrees of a quotient summing to the
/// base's.
fn degrees(graph: &WeightedGraph) -> Vec<f64> {
    (0..graph.node_count())
        .map(|node| {
            graph.adj[node].iter().map(|(_, w)| w).sum::<f64>() + 2.0 * graph.self_loops[node]
        })
        .collect()
}

/// Twice the total weight — the `2m` of the modularity formula.
fn total_degree(graph: &WeightedGraph) -> f64 {
    degrees(graph).iter().sum()
}

/// The state of one local-moving phase: which community every node is in,
/// and everything a move needs to be scored and checked. Shared by Louvain,
/// which starts it from singletons and sweeps, and Leiden, which starts it
/// from the previous round's partition and drains a queue.
struct Mover<'g> {
    graph: &'g WeightedGraph,
    resolution: f64,
    degree: Vec<f64>,
    two_m: f64,
    community: Vec<usize>,
    community_degree: Vec<f64>,
    /// The nodes of each community, in the order they arrived — a sweep
    /// over `0..n` is the visit order, this is for the cycle test.
    members: Vec<Vec<usize>>,
    scratch: CycleScratch,
    /// Calls from the node being placed into each neighbouring community.
    /// A `BTreeMap` so the candidates come out in ascending id — the
    /// tie-break.
    links: BTreeMap<usize, f64>,
}

impl<'g> Mover<'g> {
    /// Start from `community`, whose ids must all be below `slots`. The
    /// caller has checked that the graph has any weight at all.
    fn new(graph: &'g WeightedGraph, resolution: f64, community: Vec<usize>, slots: usize) -> Self {
        let degree = degrees(graph);
        let two_m = degree.iter().sum();
        let mut community_degree = vec![0.0f64; slots];
        let mut members: Vec<Vec<usize>> = vec![Vec::new(); slots];
        for (node, &own) in community.iter().enumerate() {
            community_degree[own] += degree[node];
            members[own].push(node);
        }
        Self {
            graph,
            resolution,
            degree,
            two_m,
            community,
            community_degree,
            members,
            scratch: CycleScratch::new(slots),
            links: BTreeMap::new(),
        }
    }

    /// Move `node` to the neighbouring community that gains the most, if any
    /// gains over staying. Returns whether it moved.
    ///
    /// The gain of moving `i` into community `C`, with `i` taken out of
    /// wherever it was, is `k_iC - γ · k_i · Σ_C / 2m` up to a constant
    /// factor: the calls between `i` and `C`, less what the null model expects
    /// given `i`'s degree and `C`'s total degree. Staying is a candidate like
    /// any other, scored with `i` removed, so a node in a community it no
    /// longer fits leaves it. A move needs a strictly better gain than
    /// staying, and candidates are tried in ascending community id, so ties
    /// are deterministic and lean towards leaving things be.
    ///
    /// A candidate that would put its community on a directed cycle of the
    /// quotient is skipped, however much it gains — see [`joins_a_cycle`].
    /// The graph handed in is a DAG, by construction at the base and by this
    /// rule at every rung above it, and the starting partition's quotient is
    /// one too — singletons, or a partition this rule built — so the quotient
    /// is acyclic before the first move and stays so after each.
    fn place(&mut self, node: usize) -> bool {
        let Self {
            graph,
            resolution,
            degree,
            two_m,
            community,
            community_degree,
            members,
            scratch,
            links,
        } = self;
        let (resolution, two_m) = (*resolution, *two_m);

        links.clear();
        for &(neighbour, weight) in &graph.adj[node] {
            *links.entry(community[neighbour]).or_insert(0.0) += weight;
        }

        let own = community[node];
        community_degree[own] -= degree[node];

        let gain = |candidate: usize, into: f64, community_degree: &[f64]| {
            into - resolution * degree[node] * community_degree[candidate] / two_m
        };
        let mut best = own;
        let mut best_gain = gain(
            own,
            links.get(&own).copied().unwrap_or(0.0),
            community_degree,
        );
        for (&candidate, &into) in links.iter() {
            if candidate == own {
                continue;
            }
            let candidate_gain = gain(candidate, into, community_degree);
            if candidate_gain <= best_gain + GAIN_EPSILON {
                continue;
            }
            // Better — but only legal if the quotient stays a DAG.
            let after = |other: usize| {
                if other == node {
                    candidate
                } else {
                    community[other]
                }
            };
            if joins_a_cycle(graph, after, members, &[node], candidate, scratch) {
                continue;
            }
            best = candidate;
            best_gain = candidate_gain;
        }

        community_degree[best] += degree[node];
        if best == own {
            return false;
        }
        community[node] = best;
        let at = members[own]
            .iter()
            .position(|&member| member == node)
            .expect("a node is listed in its own community");
        members[own].swap_remove(at);
        members[best].push(node);
        true
    }

    /// Louvain's driver: full sweeps in index order until one moves nothing.
    fn sweep(&mut self) {
        for _ in 0..MAX_SWEEPS {
            let mut moved = false;
            for node in 0..self.graph.node_count() {
                moved |= self.place(node);
            }
            if !moved {
                break;
            }
        }
    }

    /// Leiden's driver, the paper's "fast local move": every node once, in
    /// index order, and after that only the neighbours of a node that moved
    /// — the only nodes whose best community can have changed — each queued
    /// at most once at a time. A sweep re-scores every node after any move;
    /// this re-scores the handful that a move could have affected, and on a
    /// graph where most nodes settle at once that is most of the work saved.
    /// Deterministic for the same reason a sweep is: the queue starts in
    /// index order and grows in adjacency order.
    fn drain_queue(&mut self) {
        let n = self.graph.node_count();
        let mut queue: VecDeque<usize> = (0..n).collect();
        let mut queued = vec![true; n];
        // Strict gains make the drain finite; the budget is insurance against
        // floating-point ties chasing each other, and is a sweep's worth.
        let mut budget = n.saturating_mul(MAX_SWEEPS);
        while let Some(node) = queue.pop_front() {
            queued[node] = false;
            if budget == 0 {
                break;
            }
            budget -= 1;
            if !self.place(node) {
                continue;
            }
            let own = self.community[node];
            for &(neighbour, _) in &self.graph.adj[node] {
                if self.community[neighbour] != own && !queued[neighbour] {
                    queued[neighbour] = true;
                    queue.push_back(neighbour);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Leiden
// ---------------------------------------------------------------------------

/// One pass of the Leiden algorithm at `resolution`: rounds of move, refine
/// and collapse until every box is one node of the collapsed graph.
///
/// Returns the assignment of nodes to boxes, renumbered so they are
/// `0..count` in order of lowest member, like [`louvain_pass`], and is
/// dropped into the ladder in its place.
///
/// A round moves nodes with [`Mover::drain_queue`], starting from the
/// partition the previous round left — singletons in the first — then
/// [`refine`]s what that made, and collapses the graph by the *refined*
/// partition while handing the *unrefined* one on as the next round's start.
/// That is the whole difference from Louvain, which collapses by the
/// partition the moves made and starts the next round from singletons of it:
/// here every node of the collapsed graph is something the refinement built,
/// and it built only connected things, so what the moves regroup can only be
/// connected pieces of connected pieces. The run is done when the moves
/// leave every box a single node, which is when the partition is a fixed
/// point of both phases. It is also stopped when a refinement merges nothing
/// — the moves grouped nodes none of which are well connected to the group,
/// which the DAG rule can arrange by refusing every merge inside a box — and
/// then the answer is the current nodes as they stand, since a box the
/// refinement will not rebuild is a box the method does not stand behind.
///
/// Each round collapses at least one merge, so the rounds are bounded by the
/// node count; [`MAX_ROUNDS`] is insurance.
fn leiden_pass(graph: &WeightedGraph, resolution: f64) -> (Vec<usize>, usize) {
    let n = graph.node_count();
    if total_degree(graph) <= 0.0 {
        return renumber(&(0..n).collect::<Vec<_>>());
    }

    // The node of `current` each base node has been collapsed into.
    let mut owner: Vec<usize> = (0..n).collect();
    // `current` is `graph` in the first round and a collapse of it after.
    let mut collapsed: Option<WeightedGraph> = None;
    let mut partition: Vec<usize> = (0..n).collect();
    let mut slots = n;

    for _ in 0..MAX_ROUNDS {
        let current: &WeightedGraph = collapsed.as_ref().unwrap_or(graph);
        let mut mover = Mover::new(current, resolution, partition, slots);
        mover.drain_queue();
        let (community, count) = renumber(&mover.community);
        if count == current.node_count() {
            return renumber(
                &owner
                    .iter()
                    .map(|&node| community[node])
                    .collect::<Vec<_>>(),
            );
        }

        let (refined, refined_count) = refine(&mover);
        if refined_count == current.node_count() {
            return renumber(&owner);
        }

        let mut next = vec![0usize; refined_count];
        for (node, &sub) in refined.iter().enumerate() {
            next[sub] = community[node];
        }
        for node in owner.iter_mut() {
            *node = refined[*node];
        }
        let aggregated = aggregate(current, &refined, refined_count);
        collapsed = Some(aggregated);
        partition = next;
        slots = refined_count;
    }

    renumber(
        &owner
            .iter()
            .map(|&node| partition[node])
            .collect::<Vec<_>>(),
    )
}

/// Rebuild every box of `mover`'s partition from singletons, merging only
/// what is well connected: the refinement that makes Leiden's boxes
/// connected.
///
/// Returns the refined assignment, renumbered like every pass. Each refined
/// box lies inside one box of the partition.
///
/// Within a box `S`, a node `v` is *well connected* when its calls into the
/// rest of `S` are at least what the null model expects at this resolution,
/// `γ · k_v · (K_S − k_v) / 2m`, and a sub-box is well connected by the same
/// measure with its own degree. Nodes are visited in index order; one that is
/// still alone and well connected merges into the well-connected sub-box it
/// gains the most by joining, if any gains at all, and the sub-box's degree
/// and outward calls are updated for the next. A node that has been joined
/// by another is not alone and is not visited again — merging is one way,
/// which is what makes every sub-box a union of merges into something
/// connected, and so connected itself. The paper draws the merge at random,
/// weighted by gain; this takes the best, so the run is a function of the
/// graph alone.
///
/// The DAG rule holds here as everywhere: a merge that would put its sub-box
/// on a directed cycle of the refined quotient is refused, and refusing one
/// disconnects nothing. Any such cycle lies within one box — a cycle of the
/// refined quotient projects to a closed walk of the partition's quotient,
/// which is acyclic, so it never leaves a box — and the search is confined
/// accordingly: every node outside the box under refinement is mapped to one
/// spare group with no members, where the walk dead-ends.
fn refine(mover: &Mover) -> (Vec<usize>, usize) {
    let graph = mover.graph;
    let n = graph.node_count();
    let (resolution, two_m) = (mover.resolution, mover.two_m);
    let degree = &mover.degree;
    let community = &mover.community;

    let mut refined: Vec<usize> = (0..n).collect();
    // Index `n` is the spare group for everything outside the current box.
    let mut sub_members: Vec<Vec<usize>> = (0..n).map(|node| vec![node]).collect();
    sub_members.push(Vec::new());
    let mut sub_degree: Vec<f64> = degree.clone();
    // Calls from a sub-box to the rest of its box, and from a node to the
    // rest of its box — the well-connectedness measure, kept current.
    let mut sub_external = vec![0.0f64; n];
    let mut within = vec![0.0f64; n];
    let mut scratch = CycleScratch::new(n + 1);
    let mut links: BTreeMap<usize, f64> = BTreeMap::new();

    for (box_id, box_members) in mover.members.iter().enumerate() {
        if box_members.len() < 2 {
            continue;
        }
        let mut nodes = box_members.clone();
        nodes.sort_unstable();
        let box_degree: f64 = nodes.iter().map(|&node| degree[node]).sum();
        for &node in &nodes {
            within[node] = graph.adj[node]
                .iter()
                .filter(|(other, _)| community[*other] == box_id)
                .map(|(_, weight)| weight)
                .sum();
            sub_external[node] = within[node];
        }
        let expected =
            |own_degree: f64| resolution * own_degree * (box_degree - own_degree) / two_m;

        for &node in &nodes {
            if sub_members[node].len() != 1 || within[node] < expected(degree[node]) {
                continue;
            }
            links.clear();
            for &(other, weight) in &graph.adj[node] {
                if community[other] == box_id {
                    *links.entry(refined[other]).or_insert(0.0) += weight;
                }
            }

            let mut best = node;
            let mut best_gain = 0.0;
            for (&candidate, &into) in &links {
                if sub_external[candidate] < expected(sub_degree[candidate]) {
                    continue;
                }
                let gain = into - resolution * degree[node] * sub_degree[candidate] / two_m;
                if gain <= best_gain + GAIN_EPSILON {
                    continue;
                }
                let after = |other: usize| {
                    if other == node {
                        candidate
                    } else if community[other] != box_id {
                        n
                    } else {
                        refined[other]
                    }
                };
                if joins_a_cycle(graph, after, &sub_members, &[node], candidate, &mut scratch) {
                    continue;
                }
                best = candidate;
                best_gain = gain;
            }

            if best != node {
                let into = links[&best];
                refined[node] = best;
                sub_members[node].clear();
                sub_members[best].push(node);
                sub_degree[best] += degree[node];
                sub_external[best] += within[node] - 2.0 * into;
            }
        }
    }

    renumber(&refined)
}

// ---------------------------------------------------------------------------
// Markov clustering
// ---------------------------------------------------------------------------

/// One pass of Markov clustering at `inflation`: settle the flow over the
/// graph, then put every node in the box of the attractor it drains into.
///
/// Returns the assignment of nodes to boxes, renumbered so they are `0..count`
/// in order of lowest member, like [`louvain_pass`].
///
/// Reading the settled matrix off is where MCL's interpretation lives, and
/// where this one differs from the textbook's. An *attractor* is a node whose
/// column still holds flow to itself; attractors that feed each other are one
/// system; and every other node belongs to the system its column sends the
/// most flow to. Textbook MCL stops there, and can put a system on both ends
/// of a one-way path — the objective is flow, which runs both ways over calls
/// that run one. So the read-off is held to the same rule as a Louvain move:
/// two attractors are united only if the quotient stays a DAG, and a node
/// drains into the best system that keeps it one, falling through to the next
/// by flow and staying alone if none does. Uniting and draining are swept
/// together, each in index order, until neither moves anything, so the result
/// is a function of the flow alone and a union refused for a cycle that a
/// later drain dissolves is made after all. A node whose column reaches no
/// attractor — which only happens when [`MARKOV_MAX_ITERATIONS`] ran out
/// first — is left as a box of its own.
fn markov_pass(graph: &WeightedGraph, inflation: f64) -> (Vec<usize>, usize) {
    let n = graph.node_count();
    let flow = markov_flow(graph, inflation);
    let holds = |column: &[(usize, f64)], row: usize| {
        column.binary_search_by_key(&row, |entry| entry.0).is_ok()
    };
    let attractor: Vec<bool> = flow
        .iter()
        .enumerate()
        .map(|(node, column)| holds(column, node))
        .collect();

    let mut group: Vec<usize> = (0..n).collect();
    let mut members: Vec<Vec<usize>> = (0..n).map(|node| vec![node]).collect();
    let mut scratch = CycleScratch::new(n);

    // Attractors that feed each other are one system, and everything else
    // drains into the system it sends the most flow to, failing that into the
    // system it shares the most calls with. Settled flow is all or nothing — a
    // column holds its whole mass on one attractor — so when the flow's answer
    // would close a cycle there is no second answer in the flow to fall
    // through to, and the node's own calls are asked instead, the way the
    // singleton fold asks them at the top. `BTreeMap`s so equal weights fall
    // to the lower system, and stable sorts keep that order among ties.
    //
    // Both are swept to a fixed point together rather than once each, because
    // the cycle test is against the boxes as they stand. `a` calling `b`
    // calling `d` cannot join `d`'s box while `b` is still outside it — that
    // is a cycle between two boxes — and can once `b` is in. The same holds of
    // a union: a triangle `a → b → c` settles on `a` and `c` sharing the flow,
    // and `{a c}` cannot be one system while `b` sits outside it, calling and
    // called. Uniting first and draining after split every such triangle in
    // two; retried after the drains, the union finds `b` already in and goes
    // through.
    let mut by_flow: BTreeMap<usize, f64> = BTreeMap::new();
    let mut by_calls: BTreeMap<usize, f64> = BTreeMap::new();
    for _ in 0..MAX_SWEEPS {
        let mut moved = unite_attractors(
            graph,
            &flow,
            &attractor,
            &mut group,
            &mut members,
            &mut scratch,
        );
        for node in 0..n {
            if attractor[node] || group[node] != node {
                continue;
            }
            if drain(
                graph,
                node,
                &flow,
                &attractor,
                &mut group,
                &mut members,
                &mut scratch,
                &mut by_flow,
                &mut by_calls,
            ) {
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }

    renumber(&group)
}

/// Unite every pair of attractors that feed each other and are not yet one
/// system, where the DAG rule allows it — see [`markov_pass`]. Mutual flow
/// rather than one-way, because a column that has not quite settled can still
/// carry a trace towards an attractor it will not end up with. Returns whether
/// it united any.
fn unite_attractors(
    graph: &WeightedGraph,
    flow: &[Vec<(usize, f64)>],
    attractor: &[bool],
    group: &mut [usize],
    members: &mut [Vec<usize>],
    scratch: &mut CycleScratch,
) -> bool {
    let holds = |column: &[(usize, f64)], row: usize| {
        column.binary_search_by_key(&row, |entry| entry.0).is_ok()
    };
    let mut united = false;
    for node in 0..graph.node_count() {
        if !attractor[node] {
            continue;
        }
        for &(other, _) in &flow[node] {
            if other <= node || !attractor[other] || !holds(&flow[other], node) {
                continue;
            }
            let (mine, theirs) = (group[other], group[node]);
            if mine == theirs {
                continue;
            }
            let after = |member: usize| {
                if group[member] == mine {
                    theirs
                } else {
                    group[member]
                }
            };
            if joins_a_cycle(graph, after, members, &members[mine], theirs, scratch) {
                continue;
            }
            let moving = std::mem::take(&mut members[mine]);
            for &member in &moving {
                group[member] = theirs;
            }
            members[theirs].extend(moving);
            united = true;
        }
    }
    united
}

/// Place one node that is not an attractor into the best system the DAG rule
/// allows — see [`markov_pass`]. Returns whether it found one.
#[allow(clippy::too_many_arguments)]
fn drain(
    graph: &WeightedGraph,
    node: usize,
    flow: &[Vec<(usize, f64)>],
    attractor: &[bool],
    group: &mut [usize],
    members: &mut [Vec<usize>],
    scratch: &mut CycleScratch,
    by_flow: &mut BTreeMap<usize, f64>,
    by_calls: &mut BTreeMap<usize, f64>,
) -> bool {
    {
        by_flow.clear();
        for &(row, weight) in &flow[node] {
            if attractor[row] {
                *by_flow.entry(group[row]).or_insert(0.0) += weight;
            }
        }
        by_calls.clear();
        for &(neighbour, weight) in &graph.adj[node] {
            let system = group[neighbour];
            if attractor[system] && !by_flow.contains_key(&system) {
                *by_calls.entry(system).or_insert(0.0) += weight;
            }
        }
        let mut candidates: Vec<(usize, f64)> = by_flow.iter().map(|(&g, &w)| (g, w)).collect();
        candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut fallbacks: Vec<(usize, f64)> = by_calls.iter().map(|(&g, &w)| (g, w)).collect();
        fallbacks.sort_by(|a, b| b.1.total_cmp(&a.1));
        candidates.extend(fallbacks);
        for (candidate, _) in candidates {
            let after = |member: usize| {
                if member == node {
                    candidate
                } else {
                    group[member]
                }
            };
            if joins_a_cycle(graph, after, members, &[node], candidate, scratch) {
                continue;
            }
            // A node that is not an attractor was never united with anything
            // above, so it is still the only member of its own box.
            members[node].clear();
            group[node] = candidate;
            members[candidate].push(node);
            return true;
        }
    }
    false
}

/// The settled flow matrix, one column per node, each sorted by row and
/// summing to one — or as settled as [`MARKOV_MAX_ITERATIONS`] rounds get it.
///
/// A column starts as the node's calls, each way, plus a self-loop, all
/// normalised: where one step of a walk from the node lands. The loop is the
/// walker's chance of staying put. Without one a walk on a bipartite piece of
/// the graph never settles, and with one out of scale with the node's edges it
/// either never leaves or barely stays, so it weighs as much as the node's
/// heaviest edge, on top of whatever calls the quotient has already turned
/// inward — a box of thirty tightly knit functions should be harder to draw
/// flow out of than a box of two.
///
/// Then rounds of expansion — the column becomes the flow-weighted sum of the
/// columns it points at, which is one column of the matrix squared, computed
/// sparsely — inflation, and pruning, until no entry moves.
fn markov_flow(graph: &WeightedGraph, inflation: f64) -> Vec<Vec<(usize, f64)>> {
    let n = graph.node_count();
    let mut columns: Vec<Vec<(usize, f64)>> = (0..n)
        .map(|node| {
            let heaviest = graph.adj[node]
                .iter()
                .map(|(_, weight)| *weight)
                .fold(0.0, f64::max);
            let stay = graph.self_loops[node] + if heaviest > 0.0 { heaviest } else { 1.0 };
            // `adj` is sorted by index and never holds the node itself, so the
            // loop goes in at its place in the order.
            let mut column: Vec<(usize, f64)> = Vec::with_capacity(graph.adj[node].len() + 1);
            let mut placed = false;
            for &(neighbour, weight) in &graph.adj[node] {
                if !placed && neighbour > node {
                    column.push((node, stay));
                    placed = true;
                }
                column.push((neighbour, weight));
            }
            if !placed {
                column.push((node, stay));
            }
            normalise(&mut column);
            column
        })
        .collect();

    // Scratch for the sparse accumulation: a dense row of sums, and which of
    // its entries the column under construction has touched.
    let mut sum = vec![0.0f64; n];
    let mut touched: Vec<usize> = Vec::new();
    let mut is_touched = vec![false; n];

    for _ in 0..MARKOV_MAX_ITERATIONS {
        let mut change = 0.0f64;
        let next: Vec<Vec<(usize, f64)>> = (0..n)
            .map(|node| {
                touched.clear();
                for &(via, weight) in &columns[node] {
                    for &(row, flow) in &columns[via] {
                        if !is_touched[row] {
                            is_touched[row] = true;
                            touched.push(row);
                            sum[row] = 0.0;
                        }
                        sum[row] += weight * flow;
                    }
                }
                touched.sort_unstable();
                let mut column: Vec<(usize, f64)> = touched
                    .iter()
                    .map(|&row| {
                        is_touched[row] = false;
                        (row, sum[row].powf(inflation))
                    })
                    .collect();
                prune(&mut column);
                change = change.max(largest_difference(&columns[node], &column));
                column
            })
            .collect();
        columns = next;
        if change < MARKOV_CONVERGENCE {
            break;
        }
    }
    columns
}

/// Scale a column to sum to one.
fn normalise(column: &mut [(usize, f64)]) {
    let total: f64 = column.iter().map(|(_, weight)| weight).sum();
    if total > 0.0 {
        for (_, weight) in column.iter_mut() {
            *weight /= total;
        }
    }
}

/// Drop what inflation has starved — entries under [`MARKOV_PRUNE_THRESHOLD`]
/// of the column, and everything past the [`MARKOV_PRUNE_KEEP`] heaviest —
/// and renormalise what is left. Ties among the heaviest keep the lower row,
/// so the cut is the same every run.
fn prune(column: &mut Vec<(usize, f64)>) {
    normalise(column);
    column.retain(|(_, weight)| *weight >= MARKOV_PRUNE_THRESHOLD);
    if column.len() > MARKOV_PRUNE_KEEP {
        column.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        column.truncate(MARKOV_PRUNE_KEEP);
        column.sort_unstable_by_key(|(row, _)| *row);
    }
    normalise(column);
}

/// The largest change in any entry between two sorted columns, an entry
/// missing from one side counting as zero there.
fn largest_difference(before: &[(usize, f64)], after: &[(usize, f64)]) -> f64 {
    let mut largest = 0.0f64;
    let (mut i, mut j) = (0, 0);
    while i < before.len() || j < after.len() {
        let delta = match (before.get(i), after.get(j)) {
            (Some(&(row_b, weight_b)), Some(&(row_a, weight_a))) if row_b == row_a => {
                i += 1;
                j += 1;
                weight_b - weight_a
            }
            (Some(&(row_b, weight_b)), Some(&(row_a, _))) if row_b < row_a => {
                i += 1;
                weight_b
            }
            (Some(&(_, weight_b)), None) => {
                i += 1;
                weight_b
            }
            (_, Some(&(_, weight_a))) => {
                j += 1;
                weight_a
            }
            (None, None) => unreachable!("the loop condition keeps one side non-empty"),
        };
        largest = largest.max(delta.abs());
    }
    largest
}

/// Scratch for [`joins_a_cycle`], reused across the checks of one pass so a
/// check allocates nothing.
struct CycleScratch {
    /// Stamped rather than cleared: a group is visited when its entry carries
    /// the current stamp.
    seen: Vec<u32>,
    stamp: u32,
    queue: Vec<usize>,
}

impl CycleScratch {
    fn new(groups: usize) -> Self {
        Self {
            seen: vec![0; groups],
            stamp: 0,
            queue: Vec::new(),
        }
    }
}

/// Whether the quotient would have a directed cycle through `target` once the
/// nodes in `joining` count as its members.
///
/// `group_of` is the assignment *after* the move — it has to answer `target`
/// for every node in `joining` — and `members` is the assignment before it,
/// which is why a listed member is walked only when `group_of` still agrees it
/// belongs. The quotient before the move is acyclic, and a move adds only
/// edges that touch `target`, so any new cycle runs through `target`: the
/// search starts there and asks whether it gets back.
///
/// A breadth-first walk over the quotient, so it costs the part of the
/// program downstream of `target`, per candidate move. On a graph the size of
/// a mid-sized C++ project that is the cost to watch; the standard remedy —
/// keeping a topological order of the quotient, so a move that adds only
/// forward edges needs no search at all — is worth reaching for if it shows.
fn joins_a_cycle(
    graph: &WeightedGraph,
    group_of: impl Fn(usize) -> usize,
    members: &[Vec<usize>],
    joining: &[usize],
    target: usize,
    scratch: &mut CycleScratch,
) -> bool {
    let CycleScratch { seen, stamp, queue } = scratch;
    *stamp = stamp.wrapping_add(1);
    if *stamp == 0 {
        seen.fill(0);
        *stamp = 1;
    }
    let stamp = *stamp;

    queue.clear();
    queue.push(target);
    seen[target] = stamp;

    while let Some(group) = queue.pop() {
        let extra: &[usize] = if group == target { joining } else { &[] };
        for &node in members[group].iter().chain(extra) {
            if group_of(node) != group {
                continue;
            }
            for &successor in &graph.out[node] {
                let other = group_of(successor);
                if other == group {
                    continue;
                }
                if other == target {
                    return true;
                }
                if seen[other] != stamp {
                    seen[other] = stamp;
                    queue.push(other);
                }
            }
        }
    }
    false
}

/// How many base functions each merged group holds.
fn merged_sizes(sizes: &[usize], groups: &[usize], count: usize) -> Vec<usize> {
    let mut merged = vec![0usize; count];
    for (node, &group) in groups.iter().enumerate() {
        merged[group] += sizes[node];
    }
    merged
}

/// Fold every one-function cluster at the top of the ladder into the neighbour
/// it shares the heaviest edge with.
///
/// Returns `None` when there is nothing to fold.
///
/// A function no box gains by taking in comes through as a box of its own, and
/// at the top of the ladder that is a box holding one function beside boxes
/// holding thirty. On a real project that was `fputc`, alone, next to clusters
/// of 22 and 29 — a tenth of the map spent on a libc call, and a slot that says
/// nothing a label on an edge could not.
///
/// It is rare under modularity — a leaf's one call is usually worth more than
/// the null model expects of it — but a function whose few calls all go to a
/// very busy box can come out that way, and it is only at the last rung that
/// it has nowhere further to go. So this is the one place the objective is
/// overruled, and it is overruled by the heaviest edge. The cycle rule is not
/// overruled with it: a neighbour the fold would close a quotient cycle by
/// joining is passed over for the next heaviest, and a singleton with no legal
/// neighbour stays alone.
fn fold_singletons(graph: &WeightedGraph, sizes: &[usize]) -> Option<(Vec<usize>, usize)> {
    let n = graph.node_count();
    let mut group: Vec<usize> = (0..n).collect();
    let mut members: Vec<Vec<usize>> = (0..n).map(|node| vec![node]).collect();
    let mut scratch = CycleScratch::new(n);

    let mut folded = false;
    for (node, &size) in sizes.iter().enumerate() {
        if size != 1 {
            continue;
        }
        // Heaviest first. `adj` is sorted by index and the sort is stable, so
        // among equally heavy candidates the lowest index comes first.
        let mut candidates: Vec<(usize, f64)> = graph.adj[node].clone();
        candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
        // A cluster with no neighbours at all is a whole connected component
        // that merged into one box. There is nothing to fold it into.
        for (partner, _) in candidates {
            let (mine, theirs) = (group[node], group[partner]);
            if mine == theirs {
                // An earlier fold already put the two together.
                break;
            }
            let after = |other: usize| {
                if group[other] == mine {
                    theirs
                } else {
                    group[other]
                }
            };
            if joins_a_cycle(graph, after, &members, &members[mine], theirs, &mut scratch) {
                continue;
            }
            let moving = std::mem::take(&mut members[mine]);
            for &member in &moving {
                group[member] = theirs;
            }
            members[theirs].extend(moving);
            folded = true;
            break;
        }
    }

    if !folded {
        return None;
    }
    Some(renumber(&group))
}

fn renumber(groups: &[usize]) -> (Vec<usize>, usize) {
    let mut mapping: HashMap<usize, usize> = HashMap::new();
    let mut next = 0usize;
    let compacted = groups
        .iter()
        .map(|&group| {
            *mapping.entry(group).or_insert_with(|| {
                let assigned = next;
                next += 1;
                assigned
            })
        })
        .collect();
    (compacted, next)
}

/// Collapse each merged group into a single node, summing the weights between
/// them.
///
/// Summing is the point: it is what makes the *next* round see two groups that
/// are joined by many separate light calls as one heavy edge, which is how a
/// subsystem grows out of pair merges at all rather than the method simply
/// picking out the twenty busiest call sites in the program.
fn aggregate(graph: &WeightedGraph, groups: &[usize], count: usize) -> WeightedGraph {
    let mut adj_maps: Vec<HashMap<usize, f64>> = vec![HashMap::new(); count];
    let mut self_loops = vec![0.0f64; count];
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); count];

    for (node, &group) in groups.iter().enumerate() {
        self_loops[group] += graph.self_loops[node];
        for &(neighbour, weight) in &graph.adj[node] {
            let other = groups[neighbour];
            if other == group {
                // Seen once from each end, so halve to recover the edge's weight.
                self_loops[group] += weight / 2.0;
            } else {
                *adj_maps[group].entry(other).or_insert(0.0) += weight;
            }
        }
        for &successor in &graph.out[node] {
            let other = groups[successor];
            if other != group {
                out[group].push(other);
            }
        }
    }

    WeightedGraph::new(adj_maps, self_loops, out)
}

// ---------------------------------------------------------------------------
// Flattening the ladder
// ---------------------------------------------------------------------------

/// Turn the round-by-round assignments into the addressable [`Clustering`] the
/// rest of the app reads.
fn build_clustering(graph: &CallGraph, symbols: &[SymbolId], ladder: &[Vec<usize>]) -> Clustering {
    // Which level-`l` cluster each base node ends up in.
    let mut owners: Vec<Vec<usize>> = Vec::with_capacity(ladder.len());
    owners.push(ladder[0].clone());
    for level in &ladder[1..] {
        let previous = owners.last().expect("a level was just pushed");
        owners.push(previous.iter().map(|&group| level[group]).collect());
    }

    // Members per cluster per level. Base indices are already in symbol order,
    // so pushing in index order leaves each member list sorted.
    let mut members: Vec<Vec<Vec<usize>>> = Vec::with_capacity(owners.len());
    for (level, owner) in owners.iter().enumerate() {
        let count = ladder[level].iter().copied().max().unwrap_or(0) + 1;
        let mut buckets = vec![Vec::new(); count];
        for (node, &group) in owner.iter().enumerate() {
            buckets[group].push(node);
        }
        members.push(buckets);
    }

    // Assign ids finest-to-coarsest, clusters within a level ordered by their
    // lowest member — which, given the index order, is the lowest symbol id.
    let mut ids: Vec<Vec<Option<ClusterId>>> = Vec::with_capacity(members.len());
    let mut order: Vec<Vec<usize>> = Vec::with_capacity(members.len());
    let mut next_id = 0u32;
    let mut levels: Vec<Vec<ClusterId>> = Vec::with_capacity(members.len());

    for buckets in &members {
        let mut populated: Vec<usize> = (0..buckets.len())
            .filter(|&c| !buckets[c].is_empty())
            .collect();
        populated.sort_unstable_by_key(|&c| buckets[c][0]);

        let mut level_ids = Vec::with_capacity(populated.len());
        let mut slots = vec![None; buckets.len()];
        for &group in &populated {
            let id = ClusterId(next_id);
            next_id += 1;
            slots[group] = Some(id);
            level_ids.push(id);
        }
        levels.push(level_ids);
        ids.push(slots);
        order.push(populated);
    }

    let mut clusters: Vec<Cluster> = Vec::with_capacity(next_id as usize);
    for (level, populated) in order.iter().enumerate() {
        for &group in populated {
            let id = ids[level][group].expect("populated clusters all have ids");
            let member_symbols: Vec<SymbolId> =
                members[level][group].iter().map(|&i| symbols[i]).collect();

            let parent = ladder
                .get(level + 1)
                .and_then(|next| ids[level + 1][next[group]]);

            let children = if level == 0 {
                Vec::new()
            } else {
                order[level - 1]
                    .iter()
                    .filter(|&&child| ladder[level][child] == group)
                    .filter_map(|&child| ids[level - 1][child])
                    .collect()
            };

            // Populated clusters are the only ones given ids, so there is
            // always a lowest member for the key to anchor on.
            let key = ClusterKey {
                level,
                anchor: member_symbols[0],
            };

            let entry_point = choose_entry_point(graph, &member_symbols);
            let label = entry_point
                .and_then(|id| graph.get_function(&id))
                .map(|f| f.name.clone())
                .unwrap_or_default();

            clusters.push(Cluster {
                id,
                key,
                level,
                parent,
                children,
                members: member_symbols,
                entry_point,
                label,
            });
        }
    }

    let index: HashMap<SymbolId, ClusterId> = members[0]
        .iter()
        .enumerate()
        .filter_map(|(group, bucket)| ids[0][group].map(|id| (bucket, id)))
        .flat_map(|(bucket, id)| bucket.iter().map(move |&node| (symbols[node], id)))
        .collect();

    let mut anchors: Vec<HashMap<SymbolId, ClusterId>> = vec![HashMap::new(); levels.len()];
    for cluster in &clusters {
        anchors[cluster.level].insert(cluster.key.anchor, cluster.id);
    }

    Clustering {
        clusters,
        levels,
        index,
        anchors,
        // Both set by the caller, which is the half still holding the ladder
        // these were measured from and the settings that asked for it.
        opening: 0,
        settings: ClusterSettings::default(),
    }
}

/// Pick the member that best serves as the cluster's front door.
///
/// A merge tree has no name of its own, so the box is named after the way in.
/// A front door is entered from outside *and* leads inward, and it is the
/// second half that carries the label. Ranking purely by incoming calls from
/// outside sounds right and is not: the most-called member of almost any
/// cluster is a shared leaf. It labelled this project's networking cluster
/// `log_write` and its startup cluster `parse_integer` — both genuinely the
/// most-called members, both read as the wrong subsystem entirely.
///
/// So the ranking is how much of the cluster a member reaches from inside,
/// among the members that outside callers can actually enter through. A
/// cluster nothing calls into — the one holding `main`, typically — drops the
/// qualifier and ranks every member. Incoming calls break ties, which is where
/// that count belongs: it separates two equally central members rather than
/// choosing between a hub and a leaf.
fn choose_entry_point(graph: &CallGraph, members: &[SymbolId]) -> Option<SymbolId> {
    if members.is_empty() {
        return None;
    }

    let inside: HashSet<SymbolId> = members.iter().copied().collect();

    // Above the limit a BFS per member gets expensive, and how many members a
    // function calls directly stands in well enough for how much it fronts.
    let by_reach = members.len() <= REACHABILITY_MEMBER_LIMIT;

    // One scratch pair for the whole cluster. This is called once per cluster
    // per rung, so on a real project it runs over roughly every function at
    // every level of the ladder, and a `HashSet` and a `VecDeque` allocated
    // and dropped per member is the bulk of that.
    let mut seen: HashSet<SymbolId> = HashSet::new();
    let mut queue: VecDeque<SymbolId> = VecDeque::new();

    let ranks: Vec<Rank<'_>> = members
        .iter()
        .map(|member| {
            let callers = graph.caller_ids(member);
            Rank {
                // Multiplicity matters: five calls into a function from
                // outside make it more of an entrance than one does.
                external: callers
                    .iter()
                    .filter(|caller| !inside.contains(*caller))
                    .count(),
                dominance: if by_reach {
                    internal_reach(graph, member, &inside, &mut seen, &mut queue)
                } else {
                    graph
                        .callee_ids(member)
                        .iter()
                        .filter(|callee| inside.contains(*callee))
                        .count()
                },
                // A function nothing calls anywhere is a front door by
                // definition — `main`, a thread body, a callback the runtime
                // invokes. It has no incoming calls to rank it by, so it would
                // otherwise be filtered out of its own cluster's naming, which
                // is how the cluster holding `main` came to be labelled
                // `parse_integer`.
                unreached: callers.is_empty(),
                tie_break: tie_break_key(graph, member),
            }
        })
        .collect();

    let entered_from_outside = ranks.iter().any(|rank| rank.external > 0);

    (0..members.len())
        .filter(|&i| !entered_from_outside || ranks[i].external > 0 || ranks[i].unreached)
        .max_by(|&i, &j| {
            ranks[i]
                .dominance
                .cmp(&ranks[j].dominance)
                .then(ranks[i].external.cmp(&ranks[j].external))
                // Definition order is the tie-break a reader would use
                // themselves, so the lower one has to rank higher — hence the
                // reversed comparison against a `max_by`. The uuid behind it
                // only exists so the answer is never ambiguous.
                .then_with(|| ranks[j].tie_break.cmp(&ranks[i].tie_break))
        })
        .map(|i| members[i])
}

/// What the ranking knows about one member, read off the graph once.
///
/// The three counts used to be three passes over `members`, two of which
/// walked the same caller list; the key used to be rebuilt inside the
/// comparator, which clones a path and so paid two allocations per comparison
/// over an O(n log n) fold. Gathering all four in one pass is the same
/// ordering, read once.
struct Rank<'g> {
    /// Calls arriving from outside the cluster, one per call site.
    external: usize,
    /// How much of the cluster this member fronts — reachability, or the
    /// direct-callee stand-in above [`REACHABILITY_MEMBER_LIMIT`].
    dominance: usize,
    /// Nothing calls this member from anywhere, inside or out.
    unreached: bool,
    /// Definition site then uuid, borrowed from the graph: the path is only
    /// ever compared, so there is nothing for the key to own.
    tie_break: (&'g str, u32, uuid_bytes::Bytes),
}

/// How many other members this one reaches, following calls that stay inside
/// the cluster.
///
/// `seen` and `queue` are the caller's scratch, cleared here rather than
/// allocated: this runs once per member and the sets it builds are the size of
/// the cluster.
fn internal_reach(
    graph: &CallGraph,
    start: &SymbolId,
    inside: &HashSet<SymbolId>,
    seen: &mut HashSet<SymbolId>,
    queue: &mut VecDeque<SymbolId>,
) -> usize {
    seen.clear();
    queue.clear();
    seen.insert(*start);
    queue.push_back(*start);

    while let Some(current) = queue.pop_front() {
        for callee in graph.callee_ids(&current) {
            if inside.contains(callee) && seen.insert(*callee) {
                queue.push_back(*callee);
            }
        }
    }

    seen.len() - 1
}

fn tie_break_key<'g>(graph: &'g CallGraph, symbol: &SymbolId) -> (&'g str, u32, uuid_bytes::Bytes) {
    let location = graph.get_function(symbol).map(|f| &f.definition_location);
    (
        location.map(|l| l.file_path.as_str()).unwrap_or_default(),
        location.map(|l| l.line).unwrap_or(u32::MAX),
        uuid_bytes::Bytes(*symbol.0.as_bytes()),
    )
}

/// A thin `Ord` wrapper so the tie-break key can end in the raw uuid without
/// `logic` taking a direct dependency on the `uuid` crate.
mod uuid_bytes {
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    pub struct Bytes(pub [u8; 16]);
}

#[cfg(test)]
mod tests;
