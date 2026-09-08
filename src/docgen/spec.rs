//! Typed diagram specs — the zero-coordinate JSON contract.
//!
//! The agent never writes coordinates. A sequence diagram is participants
//! plus an ordered message list; the adapter derives every x/y from fixed
//! column arithmetic and automatic row stacking (the "technology first"
//! doctrine: geometry belongs to the engine, semantics to the author).
//!
//! Vocabulary (participant kinds, message variants) is adapted from archify
//! (MIT), whose sequence renderer is the reference implementation — minus
//! its explicit `y`/`from`/`to` coordinates, which we compute instead.

use serde::{Deserialize, Serialize};

/// The seven participant kinds archify defines. Order matters only for
/// error messages; lookups are by string.
pub const PARTICIPANT_KINDS: [&str; 7] = [
    "frontend",
    "backend",
    "database",
    "cloud",
    "security",
    "messagebus",
    "external",
];

/// Message arrow variants. `return` renders dashed and muted; `dashed` is
/// the async/trace idiom; `security` tints red; `emphasis` draws heavier.
pub const MESSAGE_VARIANTS: [&str; 5] = ["default", "emphasis", "security", "dashed", "return"];

/// A ```archify fence body: one typed diagram. `diagram_type` routes to the
/// family adapter; v1 implements `sequence`, `workflow`, `dataflow`,
/// `lifecycle`, and `architecture`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagramSpec {
    #[serde(default)]
    pub diagram_type: Option<String>,
    pub sequence: Option<SequenceSpec>,
    pub workflow: Option<WorkflowSpec>,
    pub dataflow: Option<DataflowSpec>,
    pub lifecycle: Option<LifecycleSpec>,
    pub architecture: Option<ArchitectureSpec>,
    /// Named node subsets the viewer offers as tabs (archify's guided
    /// views). Family-agnostic: `nodes` resolve against the active
    /// family's node ids — participants for sequence, nodes/states/
    /// components for the grid families.
    #[serde(default)]
    pub views: Option<Vec<View>>,
}

/// One guided view: a tab that lights the member nodes plus the routes
/// running between them (subgraph semantics), dimming the rest. `note` is
/// the story layer — one sentence of authored narrative shown as a caption
/// while the view is active (archify's guided-view note).
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub nodes: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Document-order validation of a fence's views against the active
/// family's node-id space. Unknown nodes are a fence-level problem (the
/// document falls back to a code block, same contract as spec problems).
pub fn validate_views(views: &[View], known_ids: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for view in views {
        if seen.contains(&view.id.as_str()) {
            out.push(format!("duplicate view id \"{}\"", view.id));
        }
        seen.push(view.id.as_str());
        if view.nodes.is_empty() {
            out.push(format!("view \"{}\" lists no nodes", view.id));
        }
        for node in &view.nodes {
            if !known_ids.contains(&node.as_str()) {
                out.push(format!(
                    "view \"{}\" references unknown node \"{node}\"",
                    view.id
                ));
            }
        }
    }
    out
}

/// Edge variants shared by the dataflow/lifecycle/architecture relations
/// (archify's common variant enum — the `return` idiom is sequence/workflow
/// vocabulary and is not offered here).
pub const RELATION_VARIANTS: [&str; 4] = ["default", "emphasis", "security", "dashed"];

// ---------------------------------------------------------------------------
// workflow family
//
// The swimlane contract: lanes stack vertically, columns (0..=5) are ranks
// flowing left to right. Every coordinate is derived — the author writes
// which lane and which rank, the engine solves positions, routes orthogonal
// edges, and places labels. `yOffset` (px, may be negative) is the one
// vertical escape hatch for stacking nodes that share a lane and column.
// ---------------------------------------------------------------------------

/// Route presets an edge may request; `auto` lets the router plan.
pub const EDGE_ROUTES: [&str; 7] = [
    "auto",
    "straight",
    "drop",
    "outside-right",
    "return-left",
    "bottom-channel",
    "up-channel",
];

/// Endpoint sides an edge may author; both default to automatic.
pub const ENDPOINT_SIDES: [&str; 4] = ["left", "right", "top", "bottom"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSpec {
    pub title: String,
    pub lanes: Vec<Lane>,
    pub nodes: Vec<WorkflowNode>,
    pub edges: Vec<WorkflowEdge>,
    #[serde(default)]
    pub phases: Vec<Phase>,
    #[serde(default)]
    pub groups: Vec<NodeGroup>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lane {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub variant: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkflowNode {
    pub id: String,
    pub lane: String,
    pub col: u8,
    pub label: String,
    #[serde(default)]
    pub sublabel: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub y_offset: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkflowEdge {
    #[serde(default)]
    pub id: Option<String>,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub from_side: Option<String>,
    #[serde(default)]
    pub to_side: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Phase {
    pub id: String,
    pub label: String,
    pub from_col: u8,
    pub to_col: u8,
    #[serde(default)]
    pub variant: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NodeGroup {
    pub id: String,
    pub lane: String,
    pub label: String,
    pub from_col: u8,
    pub to_col: u8,
    #[serde(default)]
    pub variant: Option<String>,
}

/// Node height model shared by validation and layout: 52px, or 68px when the
/// node carries a tag strip.
pub fn workflow_node_height(node: &WorkflowNode) -> i32 {
    if node.tag.as_deref().is_some_and(|t| !t.trim().is_empty()) {
        68
    } else {
        52
    }
}

/// Do two nodes sharing a lane and column overlap vertically (8px clearance)?
fn vertical_intervals_overlap(a: &WorkflowNode, b: &WorkflowNode, clearance: i32) -> bool {
    let da = a.y_offset.unwrap_or(0);
    let db = b.y_offset.unwrap_or(0);
    (da - db).abs() < (workflow_node_height(a) + workflow_node_height(b)) / 2 + clearance
}

/// Structural validation in document order. Geometry the solver owns (column
/// widths, route feasibility) is checked later, during layout.
pub fn validate_workflow(spec: &WorkflowSpec) -> Vec<String> {
    let mut problems = Vec::new();
    if spec.title.trim().is_empty() {
        problems.push("title must not be empty".to_string());
    }
    if spec.lanes.is_empty() {
        problems.push("a workflow needs at least 1 lane".to_string());
    }
    if spec.nodes.len() < 2 {
        problems.push(format!(
            "a workflow needs at least 2 nodes, got {}",
            spec.nodes.len()
        ));
    }
    if spec.edges.is_empty() {
        problems.push("a workflow needs at least 1 edge".to_string());
    }

    let mut lanes = std::collections::HashSet::new();
    for lane in &spec.lanes {
        if !valid_id(&lane.id) {
            problems.push(format!(
                "lane id \"{}\" must match [a-zA-Z][a-zA-Z0-9_-]*",
                lane.id
            ));
        }
        if !lanes.insert(lane.id.as_str()) {
            problems.push(format!("lane id \"{}\" is used twice", lane.id));
        }
        if lane.label.trim().is_empty() {
            problems.push(format!("lane \"{}\" must have a label", lane.id));
        }
        if let Some(v) = &lane.variant {
            if v != "exception" {
                problems.push(format!(
                    "lane \"{}\" has unknown variant \"{v}\" (only \"exception\")",
                    lane.id
                ));
            }
        }
    }

    let mut node_ids = std::collections::HashSet::new();
    for node in &spec.nodes {
        if !valid_id(&node.id) {
            problems.push(format!(
                "node id \"{}\" must match [a-zA-Z][a-zA-Z0-9_-]*",
                node.id
            ));
        }
        if !node_ids.insert(node.id.as_str()) {
            problems.push(format!("node id \"{}\" is used twice", node.id));
        }
        if !lanes.contains(node.lane.as_str()) {
            problems.push(format!(
                "node \"{}\" sits in unknown lane \"{}\"",
                node.id, node.lane
            ));
        }
        if node.col > 5 {
            problems.push(format!(
                "node \"{}\" has column {} — ranks are 0..=5",
                node.id, node.col
            ));
        }
        if !PARTICIPANT_KINDS.contains(&node.kind.as_str()) {
            problems.push(format!(
                "node \"{}\" has unknown type \"{}\" (one of: {})",
                node.id,
                node.kind,
                PARTICIPANT_KINDS.join(", ")
            ));
        }
        if node.label.trim().is_empty() {
            problems.push(format!("node \"{}\" must have a label", node.id));
        }
    }

    // Same-lane same-column collisions only (cross-column separation is the
    // solver's job).
    for (i, a) in spec.nodes.iter().enumerate() {
        for b in &spec.nodes[i + 1..] {
            if a.lane != b.lane || a.col != b.col {
                continue;
            }
            if vertical_intervals_overlap(a, b, 8) {
                problems.push(format!(
                    "nodes \"{}\" and \"{}\" share lane \"{}\" column {} and overlap vertically — separate them with yOffset",
                    a.id, b.id, a.lane, a.col
                ));
            }
        }
    }

    for edge in &spec.edges {
        let name = edge
            .id
            .clone()
            .unwrap_or_else(|| format!("{}->{}", edge.from, edge.to));
        if !node_ids.contains(edge.from.as_str()) {
            problems.push(format!("edge \"{name}\" starts at unknown node \"{}\"", edge.from));
        }
        if !node_ids.contains(edge.to.as_str()) {
            problems.push(format!("edge \"{name}\" ends at unknown node \"{}\"", edge.to));
        }
        if let Some(route) = &edge.route {
            if !EDGE_ROUTES.contains(&route.as_str()) {
                problems.push(format!(
                    "edge \"{name}\" has unknown route \"{route}\" (one of: {})",
                    EDGE_ROUTES.join(", ")
                ));
            }
        }
        for (field, side) in [("fromSide", &edge.from_side), ("toSide", &edge.to_side)] {
            if let Some(side) = side {
                if !ENDPOINT_SIDES.contains(&side.as_str()) {
                    problems.push(format!(
                        "edge \"{name}\" has unknown {field} \"{side}\" (one of: {})",
                        ENDPOINT_SIDES.join(", ")
                    ));
                }
            }
        }
        if let Some(v) = &edge.variant {
            if !MESSAGE_VARIANTS.contains(&v.as_str()) {
                problems.push(format!(
                    "edge \"{name}\" has unknown variant \"{v}\" (one of: {})",
                    MESSAGE_VARIANTS.join(", ")
                ));
            }
        }
    }

    let mut phase_ids = std::collections::HashSet::new();
    let mut phase_spans: Vec<(u8, u8, &str)> = Vec::new();
    for phase in &spec.phases {
        if !phase_ids.insert(phase.id.as_str()) {
            problems.push(format!("phase id \"{}\" is used twice", phase.id));
        }
        if phase.label.trim().is_empty() {
            problems.push(format!("phase \"{}\" must have a label", phase.id));
        }
        if phase.from_col > phase.to_col {
            problems.push(format!(
                "phase \"{}\" spans columns {}..{} backwards — fromCol must be ≤ toCol",
                phase.id, phase.from_col, phase.to_col
            ));
        }
        phase_spans.push((phase.from_col, phase.to_col, &phase.id));
    }
    phase_spans.sort();
    for pair in phase_spans.windows(2) {
        if pair[0].1 >= pair[1].0 {
            problems.push(format!(
                "phases \"{}\" and \"{}\" overlap in columns",
                pair[0].2, pair[1].2
            ));
        }
    }

    let mut group_ids = std::collections::HashSet::new();
    for group in &spec.groups {
        if !group_ids.insert(group.id.as_str()) {
            problems.push(format!("group id \"{}\" is used twice", group.id));
        }
        if !lanes.contains(group.lane.as_str()) {
            problems.push(format!(
                "group \"{}\" sits in unknown lane \"{}\"",
                group.id, group.lane
            ));
        }
        if group.from_col > group.to_col {
            problems.push(format!(
                "group \"{}\" spans columns {}..{} backwards — fromCol must be ≤ toCol",
                group.id, group.from_col, group.to_col
            ));
        }
        if group.label.trim().is_empty() {
            problems.push(format!("group \"{}\" must have a label", group.id));
        }
        if let Some(v) = &group.variant {
            if v != "security" {
                problems.push(format!(
                    "group \"{}\" has unknown variant \"{v}\" (only \"security\")",
                    group.id
                ));
            }
        }
        let has_member = spec
            .nodes
            .iter()
            .any(|n| n.lane == group.lane && n.col >= group.from_col && n.col <= group.to_col);
        if !has_member {
            problems.push(format!(
                "group \"{}\" contains no nodes — narrow its column range or drop it",
                group.id
            ));
        }
    }
    problems
}

// ---------------------------------------------------------------------------
// dataflow family
//
// The stage-pipeline contract: stages (2..=5) are vertical composition
// frames laid left to right at a fixed pitch; nodes sit on the (stage,
// row) lattice (rows 0..=4); flows connect them. Every coordinate is
// derived — the author writes which stage and which row.
// ---------------------------------------------------------------------------

/// Route presets a flow may request; `auto` lets the router plan.
pub const DATAFLOW_ROUTES: [&str; 5] = [
    "auto",
    "straight",
    "vertical-channel",
    "bottom-channel",
    "top-channel",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataflowSpec {
    pub title: String,
    pub stages: Vec<DataflowStage>,
    pub nodes: Vec<DataflowNode>,
    pub flows: Vec<DataflowFlow>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataflowStage {
    pub label: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DataflowNode {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub label: String,
    pub stage: u8,
    pub row: u8,
    #[serde(default)]
    pub sublabel: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub y_offset: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DataflowFlow {
    #[serde(default)]
    pub id: Option<String>,
    pub from: String,
    pub to: String,
    pub label: String,
    #[serde(default)]
    pub classification: Option<String>,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub from_side: Option<String>,
    #[serde(default)]
    pub to_side: Option<String>,
}

/// Fixed node height — authored sizes are a deliberate v1 divergence
/// (a fixed lattice plus authored sizes would need overlap resolution the
/// engine does not own).
pub fn dataflow_node_height() -> i32 {
    58
}

pub fn validate_dataflow(spec: &DataflowSpec) -> Vec<String> {
    let mut problems = Vec::new();
    if spec.title.trim().is_empty() {
        problems.push("title must not be empty".to_string());
    }
    if !(2..=5).contains(&spec.stages.len()) {
        problems.push(format!(
            "a dataflow needs 2..=5 stages, got {}",
            spec.stages.len()
        ));
    }
    if spec.nodes.len() < 2 {
        problems.push(format!(
            "a dataflow needs at least 2 nodes, got {}",
            spec.nodes.len()
        ));
    }
    if spec.flows.is_empty() {
        problems.push("a dataflow needs at least 1 flow".to_string());
    }

    for (i, stage) in spec.stages.iter().enumerate() {
        if stage.label.trim().is_empty() {
            problems.push(format!("stage {} must have a label", i + 1));
        }
    }

    let mut node_ids = std::collections::HashSet::new();
    for node in &spec.nodes {
        if !valid_id(&node.id) {
            problems.push(format!(
                "node id \"{}\" must match [a-zA-Z][a-zA-Z0-9_-]*",
                node.id
            ));
        }
        if !node_ids.insert(node.id.as_str()) {
            problems.push(format!("node id \"{}\" is used twice", node.id));
        }
        if node.stage as usize >= spec.stages.len() {
            problems.push(format!(
                "node \"{}\" uses invalid stage {} — valid stages are 0..={}",
                node.id,
                node.stage,
                spec.stages.len().saturating_sub(1)
            ));
        }
        if node.row > 4 {
            problems.push(format!(
                "node \"{}\" uses invalid row {} — valid rows are 0..=4",
                node.id, node.row
            ));
        }
        if !PARTICIPANT_KINDS.contains(&node.kind.as_str()) {
            problems.push(format!(
                "node \"{}\" has unknown type \"{}\" (one of: {})",
                node.id,
                node.kind,
                PARTICIPANT_KINDS.join(", ")
            ));
        }
        if node.label.trim().is_empty() {
            problems.push(format!("node \"{}\" must have a label", node.id));
        }
    }

    // Same-cell collisions: the lattice pitch is 114px against 58px nodes,
    // so only a shared (stage, row) cell can overlap (8px clearance).
    let h = dataflow_node_height();
    for (i, a) in spec.nodes.iter().enumerate() {
        for b in &spec.nodes[i + 1..] {
            if a.stage != b.stage || a.row != b.row {
                continue;
            }
            if (a.y_offset.unwrap_or(0) - b.y_offset.unwrap_or(0)).abs() < h + 8 {
                problems.push(format!(
                    "nodes \"{}\" and \"{}\" share stage {} row {} and overlap vertically — separate them with yOffset",
                    a.id, b.id, a.stage, a.row
                ));
            }
        }
    }

    for flow in &spec.flows {
        let name = flow
            .id
            .clone()
            .unwrap_or_else(|| format!("{}->{}", flow.from, flow.to));
        if !node_ids.contains(flow.from.as_str()) {
            problems.push(format!("flow \"{name}\" starts at unknown node \"{}\"", flow.from));
        }
        if !node_ids.contains(flow.to.as_str()) {
            problems.push(format!("flow \"{name}\" ends at unknown node \"{}\"", flow.to));
        }
        if flow.label.trim().is_empty() {
            problems.push(format!("flow \"{name}\" must have a label",));
        }
        if let Some(route) = &flow.route {
            if !DATAFLOW_ROUTES.contains(&route.as_str()) {
                problems.push(format!(
                    "flow \"{name}\" has unknown route \"{route}\" (one of: {})",
                    DATAFLOW_ROUTES.join(", ")
                ));
            }
        }
        check_sides_and_variant(
            &mut problems,
            "flow",
            &name,
            &flow.from_side,
            &flow.to_side,
            flow.variant.as_deref(),
        );
    }
    problems
}

// ---------------------------------------------------------------------------
// lifecycle family
//
// The state-machine contract: three fixed horizontal bands. Lane id "main"
// is reserved for the top phase band (columns 0..=4), "terminal" for the
// bottom outcome band (0..=2); every other lane shares the middle event
// band (0..=2), separated only by yOffset. Bands are dashed reading
// guides, not containers.
// ---------------------------------------------------------------------------

/// Route presets a transition may request.
pub const LIFECYCLE_ROUTES: [&str; 7] = [
    "auto",
    "straight",
    "drop",
    "bottom-channel",
    "top-channel",
    "right-channel",
    "left-channel",
];

/// The eight state kinds.
pub const STATE_TYPES: [&str; 8] = [
    "start",
    "active",
    "waiting",
    "decision",
    "success",
    "failure",
    "neutral",
    "external",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleSpec {
    pub title: String,
    pub lanes: Vec<LifecycleLane>,
    pub states: Vec<LifecycleState>,
    pub transitions: Vec<LifecycleTransition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleLane {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LifecycleState {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub label: String,
    pub lane: String,
    pub col: u8,
    #[serde(default)]
    pub sublabel: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub step: Option<String>,
    #[serde(default)]
    pub y_offset: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LifecycleTransition {
    #[serde(default)]
    pub id: Option<String>,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub from_side: Option<String>,
    #[serde(default)]
    pub to_side: Option<String>,
}

/// Which of the three fixed bands a lane renders in.
pub fn lifecycle_band(lane: &str) -> LifecycleBand {
    if lane == "main" {
        LifecycleBand::Phase
    } else if lane == "terminal" {
        LifecycleBand::Outcome
    } else {
        LifecycleBand::Event
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleBand {
    Phase,
    Event,
    Outcome,
}

impl LifecycleBand {
    /// Fixed band geometry (px): center-y, state width, state height, and
    /// the band's column count.
    pub fn geometry(self) -> (i32, i32, i32, u8) {
        match self {
            LifecycleBand::Phase => (126, 118, 62, 5),
            LifecycleBand::Event => (278, 126, 58, 3),
            LifecycleBand::Outcome => (450, 118, 58, 3),
        }
    }
}

pub fn validate_lifecycle(spec: &LifecycleSpec) -> Vec<String> {
    let mut problems = Vec::new();
    if spec.title.trim().is_empty() {
        problems.push("title must not be empty".to_string());
    }
    if spec.lanes.is_empty() || spec.lanes.len() > 4 {
        problems.push(format!(
            "a lifecycle needs 1..=4 lanes, got {}",
            spec.lanes.len()
        ));
    }
    if !spec.lanes.iter().any(|l| l.id == "main") {
        problems.push(
            "lifecycle diagrams need a lane with id \"main\" (the phase rail) — \"main\" maps to the top phase band, \"terminal\" to the bottom outcome band, other lanes share the middle event band"
                .to_string(),
        );
    }
    if spec.states.len() < 2 {
        problems.push(format!(
            "a lifecycle needs at least 2 states, got {}",
            spec.states.len()
        ));
    }

    let mut lanes = std::collections::HashSet::new();
    for lane in &spec.lanes {
        if !valid_id(&lane.id) {
            problems.push(format!(
                "lane id \"{}\" must match [a-zA-Z][a-zA-Z0-9_-]*",
                lane.id
            ));
        }
        if !lanes.insert(lane.id.as_str()) {
            problems.push(format!("lane id \"{}\" is used twice", lane.id));
        }
        if lane.label.trim().is_empty() {
            problems.push(format!("lane \"{}\" must have a label", lane.id));
        }
    }

    let mut state_ids = std::collections::HashSet::new();
    for state in &spec.states {
        if !valid_id(&state.id) {
            problems.push(format!(
                "state id \"{}\" must match [a-zA-Z][a-zA-Z0-9_-]*",
                state.id
            ));
        }
        if !state_ids.insert(state.id.as_str()) {
            problems.push(format!("state id \"{}\" is used twice", state.id));
        }
        if !lanes.contains(state.lane.as_str()) {
            problems.push(format!(
                "state \"{}\" sits in unknown lane \"{}\"",
                state.id, state.lane
            ));
            continue;
        }
        let band = lifecycle_band(&state.lane);
        let (band_y, _, state_h, cols) = band.geometry();
        if state.col >= cols {
            problems.push(format!(
                "state \"{}\" uses invalid column {} — the {} band has columns 0..={}",
                state.id,
                state.col,
                match band {
                    LifecycleBand::Phase => "phase",
                    LifecycleBand::Event => "event",
                    LifecycleBand::Outcome => "outcome",
                },
                cols - 1
            ));
        }
        // yOffset must keep the state inside the lifecycle area (64px top,
        // 122px legend reserve at the bottom of the fixed 660px canvas).
        let y = band_y - state_h / 2 + state.y_offset.unwrap_or(0);
        if y < 64 || y + state_h > 538 {
            problems.push(format!(
                "state \"{}\" with yOffset {} leaves the lifecycle area — keep it between the band rails",
                state.id,
                state.y_offset.unwrap_or(0)
            ));
        }
        if !STATE_TYPES.contains(&state.kind.as_str()) {
            problems.push(format!(
                "state \"{}\" has unknown type \"{}\" (one of: {})",
                state.id,
                state.kind,
                STATE_TYPES.join(", ")
            ));
        }
        if state.label.trim().is_empty() {
            problems.push(format!("state \"{}\" must have a label", state.id));
        }
    }

    // Event lanes share one band, so the overlap check runs across lanes:
    // with fixed widths only a shared column can collide (10px clearance).
    let rect_h = |s: &LifecycleState| lifecycle_band(&s.lane).geometry().2;
    for (i, a) in spec.states.iter().enumerate() {
        for b in &spec.states[i + 1..] {
            if lifecycle_band(&a.lane) != lifecycle_band(&b.lane) || a.col != b.col {
                continue;
            }
            if (a.y_offset.unwrap_or(0) - b.y_offset.unwrap_or(0)).abs()
                < (rect_h(a) + rect_h(b)) / 2 + 10
            {
                problems.push(format!(
                    "states \"{}\" and \"{}\" share band column {} and overlap — move one to another col or separate them with yOffset (lanes other than \"main\"/\"terminal\" share one band)",
                    a.id, b.id, a.col
                ));
            }
        }
    }

    for transition in &spec.transitions {
        let name = transition
            .id
            .clone()
            .unwrap_or_else(|| format!("{}->{}", transition.from, transition.to));
        if !state_ids.contains(transition.from.as_str()) {
            problems.push(format!(
                "transition \"{name}\" starts at unknown state \"{}\"",
                transition.from
            ));
        }
        if !state_ids.contains(transition.to.as_str()) {
            problems.push(format!(
                "transition \"{name}\" ends at unknown state \"{}\"",
                transition.to
            ));
        }
        if let Some(label) = &transition.label {
            if label.trim().is_empty() {
                problems.push(format!("transition \"{name}\" has an empty label",));
            }
        }
        if let Some(route) = &transition.route {
            if !LIFECYCLE_ROUTES.contains(&route.as_str()) {
                problems.push(format!(
                    "transition \"{name}\" has unknown route \"{route}\" (one of: {})",
                    LIFECYCLE_ROUTES.join(", ")
                ));
            }
        }
        check_sides_and_variant(
            &mut problems,
            "transition",
            &name,
            &transition.from_side,
            &transition.to_side,
            transition.variant.as_deref(),
        );
    }
    problems
}

// ---------------------------------------------------------------------------
// architecture family
//
// The deployment-view contract: a grid lattice (row/col cells, reference
// defaults 4×N with 130×64 slots) plus containment boundaries computed
// from `wraps` member lists and orthogonal connections. Authored `pos`
// and `size` are deliberately NOT part of our contract — the zero-
// coordinate rule wins over the reference's free-placement escape hatch.
// ---------------------------------------------------------------------------

/// Route presets a connection may request.
pub const ARCHITECTURE_ROUTES: [&str; 4] = ["auto", "straight", "orthogonal-h", "orthogonal-v"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchitectureSpec {
    pub title: String,
    #[serde(default)]
    pub layout: Option<ArchLayout>,
    pub components: Vec<ArchComponent>,
    #[serde(default)]
    pub boundaries: Vec<ArchBoundary>,
    #[serde(default)]
    pub connections: Vec<ArchConnection>,
}

/// Grid knobs (all optional, reference defaults apply). `origin` is not
/// offered — the engine owns placement.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ArchLayout {
    #[serde(default)]
    pub cols: Option<u8>,
    #[serde(default)]
    pub gap_x: Option<i32>,
    #[serde(default)]
    pub gap_y: Option<i32>,
    #[serde(default)]
    pub cell_w: Option<i32>,
    #[serde(default)]
    pub cell_h: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchComponent {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub label: String,
    pub row: u8,
    pub col: u8,
    #[serde(default)]
    pub sublabel: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchBoundary {
    pub kind: String,
    pub label: String,
    pub wraps: Vec<String>,
    #[serde(default)]
    pub pad: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ArchConnection {
    #[serde(default)]
    pub id: Option<String>,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub from_side: Option<String>,
    #[serde(default)]
    pub to_side: Option<String>,
}

/// Effective column count (reference default 4).
pub fn arch_cols(layout: &Option<ArchLayout>) -> u8 {
    layout.as_ref().and_then(|l| l.cols).unwrap_or(4)
}

pub fn validate_architecture(spec: &ArchitectureSpec) -> Vec<String> {
    let mut problems = Vec::new();
    if spec.title.trim().is_empty() {
        problems.push("title must not be empty".to_string());
    }
    if spec.components.is_empty() {
        problems.push("an architecture needs at least 1 component".to_string());
    }

    if let Some(layout) = &spec.layout {
        let cols = layout.cols.unwrap_or(4);
        if !(1..=12).contains(&cols) {
            problems.push(format!("layout.cols must be 1..=12, got {cols}"));
        }
        for (name, value, min) in [
            ("layout.gapX", layout.gap_x, 0),
            ("layout.gapY", layout.gap_y, 0),
            ("layout.cellW", layout.cell_w, 40),
            ("layout.cellH", layout.cell_h, 24),
        ] {
            if let Some(v) = value {
                if v < min {
                    problems.push(format!("{name} must be ≥ {min}, got {v}"));
                }
            }
        }
    }

    let cols = arch_cols(&spec.layout);
    let mut component_ids = std::collections::HashSet::new();
    let mut cells: std::collections::HashMap<(u8, u8), &str> = std::collections::HashMap::new();
    for component in &spec.components {
        if !valid_id(&component.id) {
            problems.push(format!(
                "component id \"{}\" must match [a-zA-Z][a-zA-Z0-9_-]*",
                component.id
            ));
        }
        if !component_ids.insert(component.id.as_str()) {
            problems.push(format!("component id \"{}\" is used twice", component.id));
        }
        if !PARTICIPANT_KINDS.contains(&component.kind.as_str()) {
            problems.push(format!(
                "component \"{}\" has unknown type \"{}\" (one of: {})",
                component.id,
                component.kind,
                PARTICIPANT_KINDS.join(", ")
            ));
        }
        if component.label.trim().is_empty() {
            problems.push(format!("component \"{}\" must have a label", component.id));
        }
        if component.col >= cols {
            problems.push(format!(
                "component \"{}\" col {} exceeds layout.cols {cols} (valid: 0..={})",
                component.id,
                component.col,
                cols - 1
            ));
        }
        if let Some(prev) = cells.insert((component.row, component.col), component.id.as_str()) {
            problems.push(format!(
                "components \"{prev}\" and \"{}\" share grid cell row {} col {}",
                component.id, component.row, component.col
            ));
        }
    }

    for boundary in &spec.boundaries {
        if boundary.kind != "region" && boundary.kind != "security-group" {
            problems.push(format!(
                "boundary \"{}\" has unknown kind \"{}\" (\"region\" or \"security-group\")",
                boundary.label, boundary.kind
            ));
        }
        if boundary.label.trim().is_empty() {
            problems.push(format!("boundary \"{}\" must have a label", boundary.label));
        }
        if boundary.wraps.is_empty() {
            problems.push(format!(
                "boundary \"{}\" wraps nothing — name at least one component id",
                boundary.label
            ));
        }
        for id in &boundary.wraps {
            if !component_ids.contains(id.as_str()) {
                problems.push(format!(
                    "boundary \"{}\" wraps unknown component \"{}\"",
                    boundary.label, id
                ));
            }
        }
        if let Some(pad) = boundary.pad {
            if pad < 0 {
                problems.push(format!(
                    "boundary \"{}\" pad must be ≥ 0, got {pad}",
                    boundary.label
                ));
            }
        }
    }

    for connection in &spec.connections {
        let name = connection
            .id
            .clone()
            .unwrap_or_else(|| format!("{}->{}", connection.from, connection.to));
        if !component_ids.contains(connection.from.as_str()) {
            problems.push(format!(
                "connection \"{name}\" starts at unknown component \"{}\"",
                connection.from
            ));
        }
        if !component_ids.contains(connection.to.as_str()) {
            problems.push(format!(
                "connection \"{name}\" ends at unknown component \"{}\"",
                connection.to
            ));
        }
        if let Some(label) = &connection.label {
            if label.trim().is_empty() {
                problems.push(format!("connection \"{name}\" has an empty label",));
            }
        }
        if let Some(route) = &connection.route {
            if !ARCHITECTURE_ROUTES.contains(&route.as_str()) {
                problems.push(format!(
                    "connection \"{name}\" has unknown route \"{route}\" (one of: {})",
                    ARCHITECTURE_ROUTES.join(", ")
                ));
            }
        }
        check_sides_and_variant(
            &mut problems,
            "connection",
            &name,
            &connection.from_side,
            &connection.to_side,
            connection.variant.as_deref(),
        );
    }
    problems
}

/// Endpoint sides + relation variant, shared by the three relation families.
fn check_sides_and_variant(
    problems: &mut Vec<String>,
    relation: &str,
    name: &str,
    from_side: &Option<String>,
    to_side: &Option<String>,
    variant: Option<&str>,
) {
    for (field, side) in [("fromSide", from_side), ("toSide", to_side)] {
        if let Some(side) = side {
            if !ENDPOINT_SIDES.contains(&side.as_str()) {
                problems.push(format!(
                    "{relation} \"{name}\" has unknown {field} \"{side}\" (one of: {})",
                    ENDPOINT_SIDES.join(", ")
                ));
            }
        }
    }
    if let Some(v) = variant {
        if !RELATION_VARIANTS.contains(&v) {
            problems.push(format!(
                "{relation} \"{name}\" has unknown variant \"{v}\" (one of: {})",
                RELATION_VARIANTS.join(", ")
            ));
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequenceSpec {
    pub title: String,
    pub participants: Vec<Participant>,
    pub messages: Vec<Message>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub label: String,
    #[serde(default)]
    pub sublabel: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub from: String,
    pub to: String,
    pub label: String,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Validate a parsed spec into an ordered problem list. Empty = clean.
/// Deterministic order: structure first, then participants in order, then
/// messages in order — so the same input always yields the same receipt.
pub fn validate_sequence(spec: &SequenceSpec) -> Vec<String> {
    let mut problems = Vec::new();
    if spec.title.trim().is_empty() {
        problems.push("title must not be empty".to_string());
    }
    if spec.participants.len() < 2 {
        problems.push(format!(
            "a sequence diagram needs at least 2 participants, got {}",
            spec.participants.len()
        ));
    }
    if spec.messages.is_empty() {
        problems.push("a sequence diagram needs at least 1 message".to_string());
    }

    let mut seen = std::collections::HashSet::new();
    for p in &spec.participants {
        if !valid_id(&p.id) {
            problems.push(format!(
                "participant id \"{}\" must match [a-zA-Z][a-zA-Z0-9_-]*",
                p.id
            ));
        }
        if !seen.insert(p.id.as_str()) {
            problems.push(format!("participant id \"{}\" is used twice", p.id));
        }
        if !PARTICIPANT_KINDS.contains(&p.kind.as_str()) {
            problems.push(format!(
                "participant \"{}\" has unknown type \"{}\" (one of: {})",
                p.id,
                p.kind,
                PARTICIPANT_KINDS.join(", ")
            ));
        }
        if p.label.trim().is_empty() {
            problems.push(format!("participant \"{}\" must have a label", p.id));
        }
    }

    for m in &spec.messages {
        if !seen.contains(m.from.as_str()) {
            problems.push(format!(
                "message \"{}\" references unknown source \"{}\"",
                m.label, m.from
            ));
        }
        if !seen.contains(m.to.as_str()) {
            problems.push(format!(
                "message \"{}\" references unknown target \"{}\"",
                m.label, m.to
            ));
        }
        if m.from == m.to {
            problems.push(format!(
                "message \"{}\" is a self-call — route it through another participant or fold it into a note (self-loops are not laid out yet)",
                m.label
            ));
        }
        if m.label.trim().is_empty() {
            problems.push("message labels must not be empty".to_string());
        }
        if let Some(v) = &m.variant {
            if !MESSAGE_VARIANTS.contains(&v.as_str()) {
                problems.push(format!(
                    "message \"{}\" has unknown variant \"{}\" (one of: {})",
                    m.label,
                    v,
                    MESSAGE_VARIANTS.join(", ")
                ));
            }
        }
    }
    problems
}

/// Text width in abstract units: fullwidth/East-Asian glyphs count 2, the
/// rest 1, variation selectors count 0. Same model as archify's textUnits —
/// a monospace-ish advance estimate good enough for shrink-to-fit decisions.
pub fn text_units(text: &str) -> usize {
    let mut units = 0usize;
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i] as u32;
        if (0xfe00..=0xfe0f).contains(&c) {
            i += 1;
            continue;
        }
        units += if is_fullwidth(c) { 2 } else { 1 };
        i += 1;
    }
    units.max(1)
}

/// East-Asian wide/emoji code point ranges (archify's FULLWIDTH_RE, plus the
/// surrogate-pair extensions it reaches via codePointAt).
fn is_fullwidth(c: u32) -> bool {
    matches!(c,
        0x1100..=0x115F
        | 0x231A..=0x231B
        | 0x2329..=0x232A
        | 0x23E9..=0x23EC
        | 0x23F0 | 0x23F3
        | 0x25FD..=0x25FE
        | 0x2614..=0x2615
        | 0x2630..=0x2637
        | 0x2648..=0x2653
        | 0x267F | 0x268A..=0x268F
        | 0x2693 | 0x26A1
        | 0x26AA..=0x26AB
        | 0x26BD..=0x26BE
        | 0x26C4..=0x26C5
        | 0x26CE | 0x26D4 | 0x26EA
        | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA | 0x26FD
        | 0x2705 | 0x270A..=0x270B
        | 0x2728 | 0x274C | 0x274E
        | 0x2753..=0x2755 | 0x2757
        | 0x2795..=0x2797 | 0x27B0 | 0x27BF
        | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55
        | 0x2E80..=0xA4CF
        | 0xA960..=0xA97C
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF01..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x16FE0..=0x18DFF
        | 0x1AFF0..=0x1AFFF
        | 0x1B000..=0x1B2FF
        | 0x1F000..=0x1FAFF
        | 0x20000..=0x3FFFD)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_party_spec() -> SequenceSpec {
        serde_json::from_str(
            r#"{
                "title": "T",
                "participants": [
                    {"id": "a", "type": "frontend", "label": "A"},
                    {"id": "b", "type": "backend", "label": "B"}
                ],
                "messages": [
                    {"from": "a", "to": "b", "label": "hi"}
                ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn minimal_spec_validates_clean() {
        assert!(validate_sequence(&two_party_spec()).is_empty());
    }

    #[test]
    fn validation_reports_in_document_order() {
        let bad: SequenceSpec = serde_json::from_str(
            r#"{
                "title": "",
                "participants": [
                    {"id": "1bad", "type": "nope", "label": ""},
                    {"id": "b", "type": "backend", "label": "B"}
                ],
                "messages": [
                    {"from": "ghost", "to": "b", "label": "", "variant": "nope"},
                    {"from": "b", "to": "b", "label": "self"}
                ]
            }"#,
        )
        .unwrap();
        let problems = validate_sequence(&bad);
        assert_eq!(problems.len(), 8, "{problems:?}");
        assert!(problems[0].contains("title"));
        assert!(problems[1].contains("1bad"));
        assert!(problems[2].contains("nope"));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let bad = r#"{"title":"T","participants":[{"id":"a","type":"frontend","label":"A"},
            {"id":"b","type":"backend","label":"B"}],
            "messages":[{"from":"a","to":"b","label":"hi","y":160}]}"#;
        assert!(serde_json::from_str::<SequenceSpec>(bad).is_err());
    }

    #[test]
    fn text_units_counts_fullwidth_as_two() {
        assert_eq!(text_units("abc"), 3);
        assert_eq!(text_units("读取缓存"), 8);
        assert_eq!(text_units("GET /dashboard"), 14);
        // Variation selectors are width-zero.
        assert_eq!(text_units("a\u{FE0F}"), 1);
    }

    #[test]
    fn views_validate_against_node_ids() {
        let v = |id: &str, nodes: &[&str]| View {
            id: id.to_string(),
            label: id.to_string(),
            nodes: nodes.iter().map(|s| s.to_string()).collect(),
            note: None,
        };
        assert!(validate_views(&[v("core", &["a", "b"])], &["a", "b"]).is_empty());
        // Duplicate id, empty node list, unknown node — reported in order.
        let problems =
            validate_views(&[v("core", &["a"]), v("core", &[]), v("side", &["z"])], &["a", "b"]);
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("duplicate view id \"core\""));
        assert!(problems[1].contains("lists no nodes"));
        assert!(problems[2].contains("unknown node \"z\""));
    }
}
