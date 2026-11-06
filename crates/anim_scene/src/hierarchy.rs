//! Parent validation and world transforms (spec 13.3, 13.5 step 4, 13.7).
//!
//! 13.7's hierarchy paragraph is the whole contract: validate unique ids and parent
//! references, reject cycles, visit roots in `NodeId` order and children in persisted
//! sibling order, compute `world = parent_world * local`, use the local matrix when a node
//! has no parent, reject nonfinite matrices, and produce draw extraction keys separately
//! from hierarchy order.
//!
//! This file works over a [`NodeView`] — the three fields 13.7's algorithm reads, extracted
//! from a scene — rather than over `SceneData` directly, and the reason is not tidiness.
//! `anim_project`'s types make several of 13.11's failures unconstructable: a `BTreeMap`
//! keyed by `NodeId` cannot hold two nodes with one id, and `SceneData::validate` refuses a
//! dangling parent and a cycle before a document can exist. If these checks ran on the
//! validated type, `DuplicateNode`, `MissingParent` and `HierarchyCycle` would be variants
//! no test could reach, and 13.12 asks for cycle rejection to be *tested*. A view is the
//! level at which an authored mistake is still expressible, so it is the level at which
//! refusing one is a real branch rather than a comment. [`crate::evaluator`] builds views
//! from scenes, which means the checks are still on every production path.
//!
//! The two order rules are not re-derived here: `SceneData::nodes()` is `NodeId` order for
//! roots and `children_of()` is persisted sibling order, both of which 13.7 names and
//! `anim_project` documents. What this file adds is the visit, which no authored collection
//! can express because it is a consequence of the parent relation.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anim_core::id::NodeId;

use crate::{Result, SceneError};

/// The three fields 13.7's hierarchy algorithm reads about one node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeView {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    /// The persisted position among its siblings. 13.7 makes this the child visit order,
    /// and it is a number separate from identity precisely because an operator's draw order
    /// is the thing they change without renaming a node.
    pub sibling_order: u32,
}

/// One node's world matrix, carried in the order 13.7 visits it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldEntry {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    /// `parent_world * local`, row-major; for a root, just `local`.
    pub world: [[f64; 3]; 3],
    /// Where this node came from in the slice `visit` was handed.
    ///
    /// The visit reorders, so an index is the only way to hand a caller back the authored
    /// node beside its matrix without making it look that node up — and a lookup has a miss
    /// branch, which here would be a state 13.11 has no category for: 13.8's `SceneEvent`
    /// and `EvaluatedNode` both name a node, and neither has an error meaning "the
    /// hierarchy produced an id the scene does not contain", because that is not a document
    /// defect but a bug in this file. Carrying the index makes the bug a bounds panic in a
    /// test rather than a lie in the error taxonomy.
    pub index: usize,
}

/// The identity: what "no authored transform" means, and the reason a node with no
/// `transform.*` properties still evaluates to a finite matrix.
pub const IDENTITY: [[f64; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// `a * b` for 3x3 row-major matrices.
///
/// Written out rather than taken from a maths crate because 13.8's snapshot field is
/// literally `[[f64; 3]; 3]`, this product is nine multiply-accumulates, and the order of
/// those accumulations is part of what makes two processes agree bit-for-bit. A dependency
/// whose internal evaluation order is its own business would put that guarantee behind an
/// interface. The right-handed 2.5D convention spec 30's `CameraPose` uses belongs to
/// WP15, whose matrices are 4x4 and whose own section states it.
#[must_use]
pub fn multiply(a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut out = [[0.0f64; 3]; 3];
    for (row, out_row) in out.iter_mut().enumerate() {
        for (column, cell) in out_row.iter_mut().enumerate() {
            *cell = a[row][0] * b[0][column] + a[row][1] * b[1][column] + a[row][2] * b[2][column];
        }
    }
    out
}

/// Visit a hierarchy in 13.7's order and compose every world matrix.
///
/// `local` supplies each node's authored matrix; a node absent from it is composed through
/// [`IDENTITY`], which is 13.6's "empty properties use authored defaults" applied to a
/// transform nobody authored.
///
/// The traversal is iterative and the queue holds each node's already-computed world
/// matrix, so no parent is ever looked up after the fact. Both halves matter: spec 23's
/// performance budget names a deep hierarchy, which a recursive visit would answer with a
/// stack overflow instead of a matrix, and carrying the parent's matrix with the child
/// removes the search that would otherwise make composition quadratic in the node count.
///
/// # Errors
///
/// [`SceneError::DuplicateNode`] if two entries carry one id,
/// [`SceneError::MissingParent`] if a parent is named but never given,
/// [`SceneError::HierarchyCycle`] if entries remain unvisited once the roots have run —
/// which, parents having been checked, can only mean the parent chain never reaches a root
/// — and [`SceneError::InvalidNumericValue`] if a composed matrix is not finite.
pub fn visit(
    nodes: &[NodeView],
    local: &BTreeMap<NodeId, [[f64; 3]; 3]>,
) -> Result<Vec<WorldEntry>> {
    let mut known: BTreeSet<NodeId> = BTreeSet::new();
    for node in nodes {
        if !known.insert(node.id) {
            return Err(SceneError::DuplicateNode {
                node: node.id.to_string(),
            });
        }
    }

    // Dangling parents before cycles. Both leave a node unreachable from a root, and the
    // two remedies are different sentences in a report: one names the node that is
    // missing, the other names the loop that has to be broken.
    for node in nodes {
        if let Some(parent) = node.parent {
            if !known.contains(&parent) {
                return Err(SceneError::MissingParent {
                    node: node.id.to_string(),
                    parent: parent.to_string(),
                });
            }
        }
    }

    let mut children: BTreeMap<Option<NodeId>, Vec<&NodeView>> = BTreeMap::new();
    for node in nodes {
        children.entry(node.parent).or_default().push(node);
    }
    // `sibling_order` is 13.7's rule. The id tie-break makes the comparator total even if a
    // view arrived with two siblings claiming one position — `SceneData::validate` refuses
    // that for a real scene, so the branch is unreachable rather than a licence — and a
    // comparator able to return `Equal` for two different nodes would let the visit order
    // depend on the order the input happened to be in, which is the dependence 13.7's
    // "never hash-map or filesystem enumeration" exists to forbid.
    for siblings in children.values_mut() {
        siblings.sort_by(|a, b| {
            a.sibling_order
                .cmp(&b.sibling_order)
                .then_with(|| a.id.cmp(&b.id))
        });
    }

    // One pass over the input, because the index has to be the caller's index and not the
    // order `known` happens to yield.
    let mut positions: BTreeMap<NodeId, usize> = BTreeMap::new();
    for (index, node) in nodes.iter().enumerate() {
        positions.insert(node.id, index);
    }

    let mut queue: VecDeque<(&NodeView, [[f64; 3]; 3])> = VecDeque::new();
    // Roots in `NodeId` order (13.7). `known` is a `BTreeSet` over the 16 bytes, which is
    // that order, so the rule is satisfied by the map's own iteration rather than by a
    // sort step this file could get wrong.
    for id in &known {
        if let Some(root) = nodes
            .iter()
            .find(|node| node.id == *id && node.parent.is_none())
        {
            let world = local.get(id).copied().unwrap_or(IDENTITY);
            require_finite(root.id, world)?;
            queue.push_back((root, world));
        }
    }

    let mut entries: Vec<WorldEntry> = Vec::with_capacity(nodes.len());
    let mut visited: BTreeSet<NodeId> = BTreeSet::new();
    while let Some((node, world)) = queue.pop_front() {
        if !visited.insert(node.id) {
            continue;
        }
        entries.push(WorldEntry {
            id: node.id,
            parent: node.parent,
            world,
            index: positions[&node.id],
        });
        if let Some(kids) = children.get(&Some(node.id)) {
            for child in kids {
                let own = local.get(&child.id).copied().unwrap_or(IDENTITY);
                let composed = multiply(&world, &own);
                require_finite(child.id, composed)?;
                queue.push_back((child, composed));
            }
        }
    }

    if let Some(unreached) = known.iter().find(|id| !visited.contains(id)) {
        return Err(SceneError::HierarchyCycle {
            node: unreached.to_string(),
        });
    }
    Ok(entries)
}

/// 13.7's "reject nonfinite matrices", applied at the composition rather than at the
/// snapshot: a `NaN` arriving in a matrix is a property of the authored numbers or of the
/// arithmetic, and once a snapshot is assembled the chance to say which node caused it has
/// passed.
fn require_finite(node: NodeId, world: [[f64; 3]; 3]) -> Result<()> {
    for (row, values) in world.iter().enumerate() {
        for (column, value) in values.iter().enumerate() {
            if !value.is_finite() {
                return Err(SceneError::InvalidNumericValue {
                    field: "world_transform",
                    subject: node.to_string(),
                    detail: format!(
                        "entry [{row}][{column}] is {value}, which is not finite; spec 13.7 \
                         rejects a nonfinite matrix rather than evaluating a scene with one"
                    ),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the workspace denial governs production code"
    )]

    use super::*;
    use uuid::Uuid;

    fn namespace() -> Uuid {
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap()
    }

    fn id(label: &str) -> NodeId {
        NodeId::derive(&namespace(), label.as_bytes())
    }

    fn view(label: &str, parent: Option<&str>, order: u32) -> NodeView {
        NodeView {
            id: id(label),
            parent: parent.map(id),
            sibling_order: order,
        }
    }

    /// Which label an entry carries, so the assertions below read as names.
    fn label_of(entries: &[WorldEntry], node: NodeId) -> String {
        entries
            .iter()
            .find(|e| e.id == node)
            .map(|e| e.id.to_string())
            .unwrap_or_else(|| panic!("{node} was not visited"))
    }

    fn translate(x: f64, y: f64) -> [[f64; 3]; 3] {
        [[1.0, 0.0, x], [0.0, 1.0, y], [0.0, 0.0, 1.0]]
    }

    fn locals(pairs: &[(&str, [[f64; 3]; 3])]) -> BTreeMap<NodeId, [[f64; 3]; 3]> {
        pairs.iter().map(|(l, m)| (id(l), *m)).collect()
    }

    fn world_of(entries: &[WorldEntry], label: &str) -> [[f64; 3]; 3] {
        entries
            .iter()
            .find(|e| e.id == id(label))
            .unwrap_or_else(|| panic!("{label} was not visited"))
            .world
    }

    #[test]
    fn roots_are_visited_in_node_id_order() {
        let nodes = vec![view("second", None, 0), view("first", None, 0)];
        let entries = visit(&nodes, &BTreeMap::new()).unwrap();
        let mut expected = vec![id("first"), id("second")];
        expected.sort();
        let actual: Vec<NodeId> = entries.iter().map(|e| e.id).collect();
        assert_eq!(
            actual, expected,
            "two roots are visited by identity, not by the order authored"
        );
    }

    #[test]
    fn children_are_visited_in_persisted_sibling_order() {
        // The orders are deliberately not the listing order and not ascending in the order
        // written, so a reader can tell from the result whether anything sorted at all.
        let nodes = vec![
            view("root", None, 0),
            view("cel-c", Some("root"), 9),
            view("cel-a", Some("root"), 1),
            view("cel-b", Some("root"), 5),
        ];
        let entries = visit(&nodes, &BTreeMap::new()).unwrap();
        let actual: Vec<String> = entries[1..]
            .iter()
            .map(|e| label_of(&entries, e.id))
            .collect();
        let expected: Vec<String> = ["cel-a", "cel-b", "cel-c"]
            .iter()
            .map(|l| id(l).to_string())
            .collect();
        assert_eq!(
            actual, expected,
            "sibling order 1, 5, 9 is the visit order, not the authored order"
        );
    }

    #[test]
    fn a_three_level_chain_composes_in_the_order_the_fixture_was_written_in() {
        // A view whose children are listed before their parents is the shape a `BTreeMap`
        // keyed by `NodeId` produces, and it is the shape that breaks a visit relying on
        // arrival order: the grandchild must still inherit both translations.
        let nodes = vec![
            view("grandchild", Some("child"), 0),
            view("child", Some("root"), 0),
            view("root", None, 0),
        ];
        let locals = locals(&[
            ("root", translate(2.0, 3.0)),
            ("child", translate(10.0, 0.0)),
        ]);
        let entries = visit(&nodes, &locals).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(
            world_of(&entries, "grandchild"),
            translate(12.0, 3.0),
            "parent then child, and not the other way round"
        );
    }

    #[test]
    fn a_cycle_is_rejected_and_the_node_named_is_on_it() {
        // 13.12's "cycle rejection", reachable here and not through `SceneData`, which
        // refuses a cycle at construction — the reason this module takes a view.
        let nodes = vec![view("a", Some("b"), 0), view("b", Some("a"), 1)];
        let Err(SceneError::HierarchyCycle { node }) = visit(&nodes, &BTreeMap::new()) else {
            panic!("a two-node cycle with no root must be refused, not skipped");
        };
        assert!(
            node == id("a").to_string() || node == id("b").to_string(),
            "the report names a node on the cycle: {node}"
        );
    }

    #[test]
    fn a_longer_cycle_is_still_found_because_the_queue_terminates() {
        let nodes = vec![
            view("a", Some("c"), 0),
            view("b", Some("a"), 0),
            view("c", Some("b"), 0),
        ];
        assert!(matches!(
            visit(&nodes, &BTreeMap::new()),
            Err(SceneError::HierarchyCycle { .. })
        ));
    }

    #[test]
    fn a_parent_that_is_not_in_the_scene_is_named_rather_than_reported_as_a_cycle() {
        let nodes = vec![view("orphan", Some("absent"), 0)];
        let Err(SceneError::MissingParent { node, parent }) = visit(&nodes, &BTreeMap::new())
        else {
            panic!("a dangling parent has its own remedy");
        };
        assert_eq!(node, id("orphan").to_string());
        assert_eq!(parent, id("absent").to_string());
    }

    #[test]
    fn one_identity_twice_is_a_duplicate_rather_than_a_silent_collapse() {
        let nodes = vec![view("dup", None, 0), view("dup", None, 1)];
        assert!(matches!(
            visit(&nodes, &BTreeMap::new()),
            Err(SceneError::DuplicateNode { .. })
        ));
    }

    #[test]
    fn a_nonfinite_composition_names_the_node_that_produced_it() {
        let nodes = vec![view("root", None, 0), view("child", Some("root"), 0)];
        let locals = locals(&[("root", translate(f64::INFINITY, 0.0))]);
        let Err(SceneError::InvalidNumericValue {
            field,
            subject,
            detail,
        }) = visit(&nodes, &locals)
        else {
            panic!("13.7 says reject nonfinite matrices");
        };
        assert_eq!(field, "world_transform");
        assert_eq!(subject, id("root").to_string());
        assert!(detail.contains("not finite"), "{detail}");
    }

    #[test]
    fn an_unauthored_node_composes_through_the_identity() {
        let nodes = vec![view("root", None, 0), view("child", Some("root"), 0)];
        let locals = locals(&[("root", translate(4.0, 5.0))]);
        let entries = visit(&nodes, &locals).unwrap();
        assert_eq!(world_of(&entries, "child"), world_of(&entries, "root"));
    }

    #[test]
    fn multiplication_is_parent_then_local_because_the_two_differ() {
        // `world = parent_world * local` is 13.7's sentence and matrix multiplication is
        // not commutative, so the order needs a case where getting it backwards produces a
        // visibly different number: scale-then-translate is not translate-then-scale.
        let scale = [[2.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 1.0]];
        let shift = translate(1.0, 0.0);
        assert_eq!(
            multiply(&shift, &scale),
            [[2.0, 0.0, 1.0], [0.0, 2.0, 0.0], [0.0, 0.0, 1.0]],
            "a scaled child of a shifted parent keeps the parent's offset undoubled"
        );
        assert_ne!(multiply(&shift, &scale), multiply(&scale, &shift));
    }
}
