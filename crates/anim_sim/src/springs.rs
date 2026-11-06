//! Force accumulation and fixed-iteration spring solving (spec 14.3, 14.6, 14.8).
//!
//! ## The `Spring` type, which the spec names and never declares
//!
//! 14.8 puts `pub springs: Vec<Spring>` in `SimState` and 14.7 asks the checkpoint encoder
//! to hash "particles/springs in stable ID order" — but `Spring` has no field list anywhere
//! in the document, in this section or in any later one. The survey for this package checked
//! every occurrence of the type name: WP14's secondary motion (spec 29) defines
//! `SecondaryChainComponent`, `ChainParams`, `ChainNode` and `ChainRuntime`, and never
//! mentions `Spring`, `SpringBody` or this file. So WP05 defines it, as it defined
//! `EasingCurve` in WP04 and for the same reason: something has to be here for the package
//! that consumes it to extend rather than replace.
//!
//! The shape chosen is an **edge between a kinematic anchor and a dynamic body**, because
//! that is the only structure the inputs actually contain. 14.1 says "spring chains",
//! `SpringBody.id` is a `NodeId`, and `EvaluatedNode.parent` is the one parent link a
//! snapshot carries — `SimulationInput` deliberately has no parent field, so the topology
//! comes from the node list and the per-tick anchor position from the input list. A body
//! therefore exists for every node that has a parent, and a spring for every parent-child
//! pair, with a rest length measured at initialization.
//!
//! What that means in practice, stated rather than left to be discovered: **any authored node
//! hierarchy is a spring chain in WP05.** The dynamic position is the simulation's own and
//! does not overwrite the authored transform — 14.8's `SimulationFrame` is a separate
//! extraction, and WP06 composes the two — so nothing here changes what the scene evaluator
//! reports. WP12's bones and WP14's authored chains will narrow which nodes get bodies by
//! adding the component data this package had to infer, and that is the expected pattern,
//! not a defect being corrected.
//!
//! The float discipline is [`crate::particles`]'s: `+ - * /` and `sqrt` only.

use anim_core::hash::CanonicalBytes;
use anim_core::id::NodeId;
use anim_scene::SimulationInput;

use crate::clock::SimulationClock;
use crate::particles::GRAVITY_Y;
use crate::state::SimState;
use crate::{Result, SimError, SPRING_ID_DOMAIN};

/// Spring stiffness, force per unit extension per unit mass.
///
/// A scalar rather than per-spring because WP05 authors nothing: spec 29's `ChainParams`
/// has an authored `stiffness_hz` per chain, and that number arrives with WP14's component
/// schema and its own migration step.
pub const STIFFNESS: f32 = 90.0;

/// Velocity-proportional damping applied while accumulating force.
///
/// 29's "damped by configured stiffness/damping" and "fixed critical damping" sentences are
/// WP14's authored parameters; this is the one un-authored coefficient this package needs to
/// keep a chain from ringing forever.
pub const DAMPING: f32 = 6.0;

/// The inverse mass every body gets at initialization.
///
/// One, so `force * inverse_mass` is a plain acceleration. A `0.0` inverse mass is what a
/// kinematic anchor would need, and anchors are not bodies in this model — the parent is
/// driven by the authored transform and reaches the spring through
/// [`crate::particles::anchor_positions`], which keeps the pair of roles from having two
/// ways to say the same thing.
pub const BODY_INVERSE_MASS: f32 = 1.0;

/// The stretch beyond which a body is considered to have left sane space.
///
/// Not a clamp — nothing here silently limits a value, because 14.11 wants an *error* on an
/// inconsistent state and a clamp would hide the case it asks about. It is a check that a
/// diverged chain reports itself instead of being written into a checkpoint as a plausible
/// digest of `1e30`.
pub const MAX_EXTENSION: f32 = 1.0e6;

/// One spring: a kinematic anchor node, the dynamic body hanging off it, and the length it
/// settles to (spec 14.8's `Spring`, whose fields the spec does not give).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spring {
    /// Stable identity from the pair, so the same edge in two runs is one id.
    pub id: u128,
    /// The parent node, whose position comes from the snapshot each tick.
    pub anchor: NodeId,
    /// The child node, whose position this simulation owns.
    pub body: NodeId,
    /// The distance the spring holds at, measured at initialization.
    pub rest_length: f32,
}

impl Spring {
    /// The fields that must be finite before any bit pattern is hashed.
    ///
    /// # Errors
    ///
    /// [`SimError::NonFiniteState`] naming the offending value.
    pub fn validate_finite(&self) -> Result<()> {
        if !self.rest_length.is_finite() {
            return Err(SimError::NonFiniteState {
                subject: format!("spring {}", self.id),
                field: "rest_length",
            });
        }
        Ok(())
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) -> Result<()> {
        self.validate_finite()?;
        cp.fixed_bytes(&self.id.to_le_bytes());
        cp.fixed_bytes(self.anchor.as_bytes());
        cp.fixed_bytes(self.body.as_bytes());
        cp.u32(self.rest_length.to_bits());
        Ok(())
    }
}

/// A dynamic point the simulation owns (spec 14.8, field for field).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringBody {
    /// The node this body stands for — 14.8's `NodeId` identity, and the same value
    /// `SimulationInput.node` and `Particle.emitter` use, so one node is one thing across
    /// the whole engine.
    pub id: NodeId,
    /// Current position.
    pub position: [f32; 3],
    /// Position as of the previous step.
    pub previous_position: [f32; 3],
    /// Velocity, units per second.
    pub velocity: [f32; 3],
    /// Mass reciprocal; [`BODY_INVERSE_MASS`] for everything WP05 creates.
    pub inverse_mass: f32,
    /// The per-tick accumulator, zeroed at the start of every accumulation stage (14.6's
    /// "accumulates spring forces" is a stage, not an ongoing total).
    pub force: [f32; 3],
}

impl SpringBody {
    /// Every float a digest may not be computed over blindly.
    ///
    /// # Errors
    ///
    /// [`SimError::NonFiniteState`] naming the field.
    pub fn validate_finite(&self) -> Result<()> {
        let subject = format!("body {}", self.id);
        let fields = [
            ("position[0]", self.position[0]),
            ("position[1]", self.position[1]),
            ("position[2]", self.position[2]),
            ("previous_position[0]", self.previous_position[0]),
            ("previous_position[1]", self.previous_position[1]),
            ("previous_position[2]", self.previous_position[2]),
            ("velocity[0]", self.velocity[0]),
            ("velocity[1]", self.velocity[1]),
            ("velocity[2]", self.velocity[2]),
            ("inverse_mass", self.inverse_mass),
            ("force[0]", self.force[0]),
            ("force[1]", self.force[1]),
            ("force[2]", self.force[2]),
        ];
        for (name, value) in fields {
            if !value.is_finite() {
                return Err(SimError::NonFiniteState {
                    subject: subject.clone(),
                    field: name,
                });
            }
            if name.starts_with("position") && value.abs() > MAX_EXTENSION {
                return Err(SimError::NonFiniteState {
                    subject,
                    field: name,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn encode(&self, cp: &mut CanonicalBytes) -> Result<()> {
        self.validate_finite()?;
        cp.fixed_bytes(self.id.as_bytes());
        for value in self.position {
            cp.u32(value.to_bits());
        }
        for value in self.previous_position {
            cp.u32(value.to_bits());
        }
        for value in self.velocity {
            cp.u32(value.to_bits());
        }
        cp.u32(self.inverse_mass.to_bits());
        for value in self.force {
            cp.u32(value.to_bits());
        }
        Ok(())
    }
}

/// A spring's stable identity from its ordered pair (spec 14.7's stable-ID sorting).
///
/// Derived rather than counted, for [`crate::particles::particle_id`]'s reason: a counted id
/// would depend on traversal order, and the snapshot's traversal order is 13.7's visit order
/// rather than anything a checkpoint should hash. The order of the pair is kept, so a
/// parent-to-child edge and a child-to-parent edge are different springs — they are different
/// physical statements.
#[must_use]
pub fn spring_id(anchor: &NodeId, body: &NodeId) -> u128 {
    // `with_domain` is `new()` plus a length-prefixed `text`, so an id derivation
    // whose signature is infallible can spell it directly instead of unwrapping a
    // constant it already knows is valid ASCII. The bytes are identical; see
    // anim_core::hash::CanonicalBytes::with_domain.
    let mut cp = CanonicalBytes::new();
    cp.text(SPRING_ID_DOMAIN);
    cp.fixed_bytes(anchor.as_bytes());
    cp.fixed_bytes(body.as_bytes());
    let digest = cp.digest();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    u128::from_le_bytes(bytes)
}

/// The distance between two positions.
#[must_use]
fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

/// Zero every body's accumulator, which is 14.6's stage boundary made explicit.
pub fn clear_forces(state: &mut SimState) {
    for body in &mut state.bodies {
        body.force = [0.0; 3];
    }
}

/// Accumulate spring force into every body (spec 14.6's fourth stage).
///
/// Hooke along the axis plus damping against the body's own velocity. Gravity is *not*
/// here: a constant acceleration is not a spring force, and 14.8's `force` field is the
/// spring accumulator, so mixing the two would make a checkpoint's `force` mean two things
/// depending on which stage wrote it. Gravity is applied in [`integrate`], which is also
/// what [`crate::particles`] does with it, and one engine has one convention.
///
/// The anchor position is read from this tick's snapshot inputs, so an animated parent
/// drags its chain — the whole point of a spring chain — and nothing in this function
/// consults a mutable time or an accumulated second.
///
/// # Errors
///
/// [`SimError::UnknownEventTarget`] for a spring whose anchor or body this tick cannot
/// supply, which is a topology disagreement rather than a physics failure, and
/// [`SimError::NonFiniteState`] if an accumulated force is no longer finite.
pub fn accumulate_forces(
    state: &mut SimState,
    anchors: &std::collections::BTreeMap<NodeId, [f32; 3]>,
) -> Result<()> {
    let positions = body_index(state);
    for spring in &state.springs {
        let anchor_position =
            anchors
                .get(&spring.anchor)
                .ok_or_else(|| SimError::UnknownEventTarget {
                    event: format!("spring {}", spring.id),
                    detail: "its anchor node is not in this tick's snapshot, so the authored \
                         hierarchy and the simulation topology disagree"
                        .to_string(),
                })?;
        let body_at = *positions
            .get(&spring.body)
            .ok_or_else(|| SimError::UnknownEventTarget {
                event: format!("spring {}", spring.id),
                detail: "its body node has no SpringBody, so a spring points at nothing"
                    .to_string(),
            })?;
        let delta = [
            state.bodies[body_at].position[0] - anchor_position[0],
            state.bodies[body_at].position[1] - anchor_position[1],
            state.bodies[body_at].position[2] - anchor_position[2],
        ];
        let length = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
        let extension = length - spring.rest_length;
        // A zero-length spring has no axis. Holding still rather than dividing by zero is
        // the defined answer, and the case is reachable: a rest length of zero means the
        // child was authored on top of its parent.
        let unit = if length == 0.0 {
            [0.0; 3]
        } else {
            [delta[0] / length, delta[1] / length, delta[2] / length]
        };
        let velocity = state.bodies[body_at].velocity;
        let body = &mut state.bodies[body_at];
        for index in 0..3 {
            body.force[index] += -STIFFNESS * extension * unit[index] - DAMPING * velocity[index];
        }
    }
    for body in &mut state.bodies {
        body.validate_finite()?;
    }
    Ok(())
}

/// The node-to-slot map both force stages need, in one place.
///
/// Built from [`SimState::bodies`]' sortedness rather than searched linearly per spring:
/// `position()` inside a loop is `O(n²)` over a chain, and the map also makes the index
/// provably the body the spring names instead of an `unwrap_or(len - 1)` that would quietly
/// accumulate onto the wrong object.
fn body_index(state: &SimState) -> std::collections::BTreeMap<NodeId, usize> {
    state
        .bodies
        .iter()
        .enumerate()
        .map(|(index, body)| (body.id, index))
        .collect()
}

/// One integration step of every body: gravity, then `a = F * inverse_mass`, then velocity,
/// then position (spec 14.6's fifth stage).
pub fn integrate(state: &mut SimState, clock: &SimulationClock) {
    let dt = clock.step_seconds() as f32;
    for body in &mut state.bodies {
        body.previous_position = body.position;
        body.velocity[1] += GRAVITY_Y * dt;
        for (index, velocity) in body.velocity.iter_mut().enumerate() {
            *velocity += body.force[index] * body.inverse_mass * dt;
        }
        for (index, position) in body.position.iter_mut().enumerate() {
            *position += body.velocity[index] * dt;
        }
    }
}

/// The fixed-iteration position solve (spec 14.6's sixth stage, 14.5 step 6's "fixed solver
/// counts").
///
/// Each pass projects every body back onto its spring's rest length, measured from this
/// tick's authored anchor position, and `iterations` passes run over the springs in their
/// sorted order. The count is fixed and the order is sorted, so the result does not depend
/// on how many frames the caller presented, how many checkpoints exist, or what happened on
/// the previous tick — which is 14.1's four independencies inside one loop.
///
/// The anchor is never moved. It is the authored transform, not simulation state, and 14.8's
/// "mutation is confined to initialization, step_once and restore" is what a solver that
/// pushed a parent would violate: two children of one node would each drag the shared parent
/// and the pair would then depend on spring order.
///
/// # Errors
///
/// [`SimError::InvalidSolverCount`] for a count this build refuses,
/// [`SimError::UnknownEventTarget`] for a spring with no anchor input, and
/// [`SimError::NonFiniteState`] if a projection produced a position outside sane space.
pub fn solve(
    state: &mut SimState,
    anchors: &std::collections::BTreeMap<NodeId, [f32; 3]>,
    iterations: u16,
) -> Result<()> {
    if iterations == 0 || iterations > crate::MAX_SOLVER_ITERATIONS {
        return Err(SimError::InvalidSolverCount { count: iterations });
    }
    let positions = body_index(state);
    for _ in 0..iterations {
        for spring in &state.springs {
            spring.validate_finite()?;
            let anchor_position =
                anchors
                    .get(&spring.anchor)
                    .ok_or_else(|| SimError::UnknownEventTarget {
                        event: format!("spring {}", spring.id),
                        detail: "its anchor node is not in this tick's snapshot".to_string(),
                    })?;
            let body_at =
                *positions
                    .get(&spring.body)
                    .ok_or_else(|| SimError::UnknownEventTarget {
                        event: format!("spring {}", spring.id),
                        detail: "its body node has no SpringBody".to_string(),
                    })?;
            let delta = [
                state.bodies[body_at].position[0] - anchor_position[0],
                state.bodies[body_at].position[1] - anchor_position[1],
                state.bodies[body_at].position[2] - anchor_position[2],
            ];
            let length = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
            if length == 0.0 {
                // Nothing to project: the body is on its anchor and a rest length of zero
                // is already satisfied, so a division here would be the only outcome.
                continue;
            }
            let scale = spring.rest_length / length;
            let body = &mut state.bodies[body_at];
            for (index, value) in delta.iter().enumerate() {
                body.position[index] = anchor_position[index] + value * scale;
            }
            body.validate_finite()?;
        }
    }
    Ok(())
}

/// The topology-derived body and spring lists for one snapshot (spec 14.8's initialization
/// sentence, 14.5 step 4's canonical initial state).
///
/// Bodies are the nodes that have a parent; springs are those parent-child edges, with the
/// rest length measured from this snapshot's own transforms. Measured rather than authored
/// because nothing in 12.8 or 13.8 authors a length, and a chain whose rest length is the
/// distance it starts at is a chain that is at rest at tick zero — which is the only initial
/// state two fresh processes can be expected to agree on.
///
/// # Errors
///
/// [`SimError::NonFiniteState`] if an evaluated transform is not finite, which would make
/// every derived length non-finite with it.
pub fn initial_chains(
    nodes: &[anim_scene::EvaluatedNode],
    inputs: &[SimulationInput],
) -> Result<(Vec<SpringBody>, Vec<Spring>)> {
    let anchors = crate::particles::anchor_positions(inputs);
    let mut bodies: Vec<SpringBody> = Vec::new();
    let mut springs: Vec<Spring> = Vec::new();
    for child in nodes {
        let Some(parent) = child.parent else { continue };
        let position = [
            child.world_transform[0][2] as f32,
            child.world_transform[1][2] as f32,
            child.world_transform[2][2] as f32,
        ];
        for (index, value) in position.iter().enumerate() {
            if !value.is_finite() {
                return Err(SimError::NonFiniteState {
                    subject: format!("node {}", child.id),
                    field: ["world[0][2]", "world[1][2]", "world[2][2]"][index],
                });
            }
        }
        bodies.push(SpringBody {
            id: child.id,
            position,
            previous_position: position,
            velocity: [0.0; 3],
            inverse_mass: BODY_INVERSE_MASS,
            force: [0.0; 3],
        });
        // The parent's position comes from the same snapshot, through the input list this
        // crate's per-tick anchors also use, so a chain measures against the value it will
        // later be driven by. Refused rather than defaulted: a parent that is not in the
        // inputs would make `accumulate_forces` fail one tick later with a rest length
        // already baked into a checkpoint, and a length measured against the child's own
        // position is a zero that means nothing.
        let parent_position =
            anchors
                .get(&parent)
                .copied()
                .ok_or_else(|| SimError::UnknownEventTarget {
                    event: format!("node {}", child.id),
                    detail: "its parent is not among this snapshot's simulation inputs, so the \
                         rest length has nothing to be measured from"
                        .to_string(),
                })?;
        let rest_length = distance(position, parent_position);
        springs.push(Spring {
            id: spring_id(&parent, &child.id),
            anchor: parent,
            body: child.id,
            rest_length,
        });
    }
    bodies.sort_by_key(|body| body.id);
    springs.sort_by_key(|spring| spring.id);
    Ok((bodies, springs))
}

/// Report a repeated body or spring identity, the way
/// [`crate::particles::check_unique_ids`] does for particles.
///
/// # Errors
///
/// [`SimError::DuplicateId`] for the first repeat in either list.
pub fn check_unique_chains(state: &SimState) -> Result<()> {
    for pair in state.bodies.windows(2) {
        if pair[0].id == pair[1].id {
            return Err(SimError::DuplicateId {
                kind: "body",
                id: pair[0].id.to_string(),
            });
        }
    }
    for pair in state.springs.windows(2) {
        if pair[0].id == pair[1].id {
            return Err(SimError::DuplicateId {
                kind: "spring",
                id: pair[0].id.to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;
    use anim_core::time::FrameRate;
    use anim_scene::EvaluatedNode;

    fn node(label: &str) -> NodeId {
        NodeId::derive(
            &uuid::Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
            label.as_bytes(),
        )
    }

    fn translated(x: f64, y: f64) -> [[f64; 3]; 3] {
        [[1.0, 0.0, x], [0.0, 1.0, y], [0.0, 0.0, 1.0]]
    }

    fn evaluated(label: &str, parent: Option<&str>, x: f64, y: f64) -> EvaluatedNode {
        EvaluatedNode {
            id: node(label),
            parent: parent.map(node),
            world_transform: translated(x, y),
            z: 0.0,
            opacity: 1.0,
            visible: true,
            asset: None,
        }
    }

    fn input(label: &str, x: f64, y: f64) -> SimulationInput {
        SimulationInput {
            node: node(label),
            world_transform: translated(x, y),
            z: 0.0,
            visible: true,
        }
    }

    #[test]
    fn a_spring_id_is_ordered_and_derived() {
        let parent = node("parent");
        let child = node("child");
        assert_eq!(spring_id(&parent, &child), spring_id(&parent, &child));
        assert_ne!(
            spring_id(&parent, &child),
            spring_id(&child, &parent),
            "a parent-to-child edge and the reverse are different physical statements"
        );
    }

    #[test]
    fn bodies_are_the_children_and_rest_lengths_come_from_the_snapshot() {
        let nodes = vec![
            evaluated("root", None, 0.0, 0.0),
            evaluated("arm", Some("root"), 3.0, 4.0),
            evaluated("hand", Some("arm"), 3.0, 9.0),
        ];
        let inputs = vec![
            input("root", 0.0, 0.0),
            input("arm", 3.0, 4.0),
            input("hand", 3.0, 9.0),
        ];
        let (bodies, springs) = initial_chains(&nodes, &inputs).unwrap();
        assert_eq!(
            bodies.len(),
            2,
            "the root has no parent, so it is not a body"
        );
        assert_eq!(springs.len(), 2);
        let arm = springs
            .iter()
            .find(|spring| spring.body == node("arm"))
            .unwrap();
        // 3-4-5: the distance from (0,0) to (3,4) is exactly 5, computed here as an
        // independent oracle rather than read back from `distance`.
        assert!((arm.rest_length - 5.0).abs() < 1e-5, "{}", arm.rest_length);
        let hand = springs
            .iter()
            .find(|spring| spring.body == node("hand"))
            .unwrap();
        assert!((hand.rest_length - 5.0).abs() < 1e-5);
        assert!(
            bodies.windows(2).all(|pair| pair[0].id < pair[1].id),
            "sorted by node id, which is 14.8's stable-ID order"
        );
        assert!(
            bodies.windows(2).all(|pair| pair[0].velocity == [0.0; 3]),
            "a chain starts at rest"
        );
    }

    #[test]
    fn a_chain_at_rest_holds_its_place() {
        let nodes = vec![
            evaluated("root", None, 0.0, 0.0),
            evaluated("arm", Some("root"), 0.0, 5.0),
        ];
        let inputs = vec![input("root", 0.0, 0.0), input("arm", 0.0, 5.0)];
        let (bodies, springs) = initial_chains(&nodes, &inputs).unwrap();
        let mut state = crate::state::test_support::state();
        state.bodies = bodies;
        state.springs = springs;
        let clock = SimulationClock::new(FrameRate::new(24, 1).unwrap(), 24).unwrap();
        let anchors = crate::particles::anchor_positions(&inputs);
        accumulate_forces(&mut state, &anchors).unwrap();
        // One body, one spring at exactly its rest length, so the only force is damping
        // against a zero velocity -- which is zero. The spring really is at rest.
        assert_eq!(state.bodies[0].force, [0.0; 3]);
        clear_forces(&mut state);
        assert_eq!(
            state.bodies[0].force, [0.0; 3],
            "the stage boundary is real"
        );
        // Gravity is an acceleration applied in the integration stage, not a force.
        integrate(&mut state, &clock);
        assert!(
            state.bodies[0].velocity[1] < 0.0,
            "and it is the only thing that moved the body: {:?}",
            state.bodies[0].velocity
        );
        solve(&mut state, &anchors, 4).unwrap();
        let arm = &state.bodies[0];
        assert!(
            (distance(arm.position, anchors[&node("root")]) - 5.0).abs() < 1e-5,
            "the solve put the body back on the sphere of radius rest_length: {:?}",
            arm.position
        );
    }

    #[test]
    fn the_solver_projection_lands_on_the_rest_length_and_never_moves_the_anchor() {
        let mut state = crate::state::test_support::state();
        state.bodies = vec![SpringBody {
            id: node("arm"),
            position: [0.0, 10.0, 0.0],
            previous_position: [0.0, 10.0, 0.0],
            velocity: [0.0; 3],
            inverse_mass: BODY_INVERSE_MASS,
            force: [0.0; 3],
        }];
        state.springs = vec![Spring {
            id: spring_id(&node("root"), &node("arm")),
            anchor: node("root"),
            body: node("arm"),
            rest_length: 2.0,
        }];
        let mut anchors = std::collections::BTreeMap::new();
        anchors.insert(node("root"), [1.0, 0.0, 0.0]);

        solve(&mut state, &anchors, 1).unwrap();
        let arm = &state.bodies[0];
        assert!(
            (distance(arm.position, anchors[&node("root")]) - 2.0).abs() < 1e-5,
            "one pass puts the body on the sphere, whatever it took to get there: {:?}",
            arm.position
        );
        let further = arm.position;
        solve(&mut state, &anchors, 8).unwrap();
        assert_eq!(
            state.bodies[0].position, further,
            "a pass that finds the constraint already satisfied changes nothing, which is \
             why the iteration count is a quality dial and not a source of drift"
        );
        assert_eq!(anchors[&node("root")], [1.0, 0.0, 0.0]);
        assert_eq!(
            state.bodies.len(),
            1,
            "the anchor is authored state, not a body, and no pass may become a second \
             writer for it"
        );
    }

    #[test]
    fn an_illegal_solver_count_is_refused_before_any_work() {
        let mut state = crate::state::test_support::state();
        let anchors = std::collections::BTreeMap::new();
        assert!(matches!(
            solve(&mut state, &anchors, 0),
            Err(SimError::InvalidSolverCount { count: 0 })
        ));
        assert!(matches!(
            solve(&mut state, &anchors, crate::MAX_SOLVER_ITERATIONS + 1),
            Err(SimError::InvalidSolverCount { .. })
        ));
    }

    #[test]
    fn a_body_that_leaves_sane_space_is_reported() {
        let mut body = SpringBody {
            id: node("arm"),
            position: [0.0; 3],
            previous_position: [0.0; 3],
            velocity: [0.0; 3],
            inverse_mass: BODY_INVERSE_MASS,
            force: [0.0; 3],
        };
        body.validate_finite().expect("zero is finite and small");
        body.position[0] = f32::NAN;
        assert!(matches!(
            body.validate_finite(),
            Err(SimError::NonFiniteState {
                field: "position[0]",
                ..
            })
        ));
        body.position[0] = MAX_EXTENSION * 2.0;
        assert!(
            matches!(body.validate_finite(), Err(SimError::NonFiniteState { .. })),
            "14.11's inconsistent state covers a diverged chain as well as a NaN"
        );
    }

    #[test]
    fn a_missing_anchor_is_a_topology_error_not_a_physics_one() {
        let mut state = crate::state::test_support::state();
        state.bodies = vec![SpringBody {
            id: node("arm"),
            position: [0.0, 5.0, 0.0],
            previous_position: [0.0, 5.0, 0.0],
            velocity: [0.0; 3],
            inverse_mass: BODY_INVERSE_MASS,
            force: [0.0; 3],
        }];
        state.springs = vec![Spring {
            id: spring_id(&node("ghost"), &node("arm")),
            anchor: node("ghost"),
            body: node("arm"),
            rest_length: 5.0,
        }];
        let anchors = std::collections::BTreeMap::new();
        assert!(matches!(
            accumulate_forces(&mut state, &anchors),
            Err(SimError::UnknownEventTarget { .. })
        ));
    }

    #[test]
    fn a_duplicated_body_is_reported() {
        let body = SpringBody {
            id: node("arm"),
            position: [0.0; 3],
            previous_position: [0.0; 3],
            velocity: [0.0; 3],
            inverse_mass: BODY_INVERSE_MASS,
            force: [0.0; 3],
        };
        let mut state = crate::state::test_support::state();
        state.bodies = vec![body, body];
        assert!(matches!(
            check_unique_chains(&state),
            Err(SimError::DuplicateId { kind: "body", .. })
        ));
    }

    #[test]
    fn body_floats_are_bit_patterns_after_the_finite_check() {
        let body = SpringBody {
            id: node("arm"),
            position: [1.5, 0.0, 0.0],
            previous_position: [0.0; 3],
            velocity: [0.0; 3],
            inverse_mass: 1.0,
            force: [0.0; 3],
        };
        let mut cp = CanonicalBytes::new();
        body.encode(&mut cp).unwrap();
        // 16 id, then nine f32 positions and velocities, then inverse_mass, then three force.
        assert_eq!(cp.as_slice().len(), 16 + 9 * 4 + 4 + 3 * 4);
        assert_eq!(&cp.as_slice()[16..20], &1.5f32.to_bits().to_le_bytes());
        let mut broken = body;
        broken.velocity[2] = f32::NAN;
        let mut cp = CanonicalBytes::new();
        assert!(broken.encode(&mut cp).is_err());
    }
}
