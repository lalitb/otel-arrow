// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Node wiring contracts used to validate connection-level topology constraints.

use crate::error::Error;
use crate::node::NodeName;
use otap_df_config::PortName;

/// Per-output fanout rule for a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFanoutRule {
    /// No destination-count limit per output.
    #[default]
    Unrestricted,
    /// The number of destinations per output must be <= this limit.
    AtMostPerOutput(usize),
}

/// Delivery-completion requirement declared by a node type.
///
/// This is a generic, component-agnostic statement about what a node needs from
/// the topology it is wired into. The engine and controller reason about the
/// rule alone; no component type or URN is special-cased.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeliveryCompletionRule {
    /// The node makes no delivery-completion demand on its outbound routes.
    #[default]
    Unrestricted,
    /// Every outbound route from this node must be able to produce one
    /// aggregate delivery completion whose Ack means a nonempty set of required
    /// destinations all acked.
    ///
    /// Topology validation must reject the configuration when any route from
    /// this node cannot be proven to provide that guarantee.
    AggregateAckRequired,
}

/// How an intermediate processor handles an upstream aggregate-Ack contract.
///
/// A processor on a route from a [`DeliveryCompletionRule::AggregateAckRequired`]
/// node must explicitly declare one of the proven variants. Registration alone
/// is not evidence: a processor may acknowledge upstream before its downstream
/// work completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AggregateAckPropagation {
    /// The processor has not declared a safe aggregate-Ack propagation
    /// contract and cannot appear on an aggregate-Ack-required route.
    #[default]
    Unproven,
    /// The processor guarantees that an upstream Ack represents terminal
    /// completion of every downstream output it treats as required.
    ///
    /// An intentional local semantic decision that creates no downstream work,
    /// such as filtering every record, may also complete successfully.
    Guaranteed,
    /// The guarantee applies only when a string-valued node configuration field
    /// at `json_pointer` exactly equals `expected_value`.
    GuaranteedWhenConfigEquals {
        /// RFC 6901 JSON pointer into the node-specific configuration.
        json_pointer: &'static str,
        /// Required string value at `json_pointer`.
        expected_value: &'static str,
    },
}

/// Contract describing wiring constraints for a node type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WiringContract {
    /// Constraint on per-output destination fanout.
    pub output_fanout: OutputFanoutRule,
    /// Delivery-completion guarantee this node requires from its routes.
    pub delivery_completion: DeliveryCompletionRule,
    /// Aggregate-Ack propagation guarantee provided by this node when it is an
    /// intermediate processor.
    pub aggregate_ack_propagation: AggregateAckPropagation,
}

impl WiringContract {
    /// Unrestricted wiring contract (no per-output destination limit).
    pub const UNRESTRICTED: Self = Self {
        output_fanout: OutputFanoutRule::Unrestricted,
        delivery_completion: DeliveryCompletionRule::Unrestricted,
        aggregate_ack_propagation: AggregateAckPropagation::Unproven,
    };

    /// Creates an unrestricted wiring contract.
    #[must_use]
    pub const fn unrestricted() -> Self {
        Self::UNRESTRICTED
    }

    /// Creates a contract with a per-output destination cap.
    #[must_use]
    pub const fn at_most_per_output(max: usize) -> Self {
        Self {
            output_fanout: OutputFanoutRule::AtMostPerOutput(max),
            delivery_completion: DeliveryCompletionRule::Unrestricted,
            aggregate_ack_propagation: AggregateAckPropagation::Unproven,
        }
    }

    /// Returns this contract with an added aggregate-Ack delivery requirement.
    ///
    /// Fanout constraints are unchanged: a node can require aggregate Ack while
    /// keeping unrestricted direct fanout.
    #[must_use]
    pub const fn requiring_aggregate_ack(self) -> Self {
        Self {
            output_fanout: self.output_fanout,
            delivery_completion: DeliveryCompletionRule::AggregateAckRequired,
            aggregate_ack_propagation: self.aggregate_ack_propagation,
        }
    }

    /// Returns this contract with an unconditional aggregate-Ack propagation
    /// guarantee for use by an intermediate processor.
    #[must_use]
    pub const fn propagating_aggregate_ack(self) -> Self {
        Self {
            output_fanout: self.output_fanout,
            delivery_completion: self.delivery_completion,
            aggregate_ack_propagation: AggregateAckPropagation::Guaranteed,
        }
    }

    /// Returns this contract with a configuration-gated aggregate-Ack
    /// propagation guarantee.
    #[must_use]
    pub const fn propagating_aggregate_ack_when_config_equals(
        self,
        json_pointer: &'static str,
        expected_value: &'static str,
    ) -> Self {
        Self {
            output_fanout: self.output_fanout,
            delivery_completion: self.delivery_completion,
            aggregate_ack_propagation: AggregateAckPropagation::GuaranteedWhenConfigEquals {
                json_pointer,
                expected_value,
            },
        }
    }

    /// Whether this node requires aggregate-Ack delivery completion.
    #[must_use]
    pub const fn requires_aggregate_ack(&self) -> bool {
        matches!(
            self.delivery_completion,
            DeliveryCompletionRule::AggregateAckRequired
        )
    }

    /// Validates that this node configuration provides the declared
    /// aggregate-Ack propagation guarantee.
    pub fn validate_aggregate_ack_propagation(
        &self,
        config: &serde_json::Value,
    ) -> Result<(), String> {
        match self.aggregate_ack_propagation {
            AggregateAckPropagation::Unproven => {
                Err("its factory does not declare safe aggregate-Ack propagation".to_owned())
            }
            AggregateAckPropagation::Guaranteed => Ok(()),
            AggregateAckPropagation::GuaranteedWhenConfigEquals {
                json_pointer,
                expected_value,
            } => match config.pointer(json_pointer) {
                Some(serde_json::Value::String(actual)) if actual == expected_value => Ok(()),
                Some(serde_json::Value::String(_)) => Err(format!(
                    "aggregate-Ack propagation requires config `{json_pointer}` to equal `{expected_value}`, but it has a different value"
                )),
                Some(_) => Err(format!(
                    "aggregate-Ack propagation requires config `{json_pointer}` to equal string `{expected_value}`, but it is not a string"
                )),
                None => Err(format!(
                    "aggregate-Ack propagation requires config `{json_pointer}` to equal `{expected_value}`, but the field is absent"
                )),
            },
        }
    }

    /// Validates a source output against this contract.
    pub fn validate_output_destinations(
        &self,
        node: &NodeName,
        output: &PortName,
        destinations: &[NodeName],
    ) -> Result<(), Error> {
        match self.output_fanout {
            OutputFanoutRule::Unrestricted => Ok(()),
            OutputFanoutRule::AtMostPerOutput(max) if destinations.len() <= max => Ok(()),
            OutputFanoutRule::AtMostPerOutput(max) => Err(Error::InvalidNodeWiring {
                node: node.clone(),
                output: output.clone(),
                max_destinations: max,
                actual_destinations: destinations.to_vec(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AggregateAckPropagation, DeliveryCompletionRule, OutputFanoutRule, WiringContract,
    };
    use serde_json::json;

    /// Scenario: contracts are built with the existing constructors and with the
    /// new aggregate-Ack requirement.
    /// Guarantees: the delivery-completion requirement defaults to unrestricted
    /// for every pre-existing constructor, and `requiring_aggregate_ack`
    /// preserves the node's fanout rule so direct fanout stays unrestricted.
    #[test]
    fn delivery_completion_defaults_to_unrestricted_and_composes_with_fanout() {
        assert_eq!(
            WiringContract::default().delivery_completion,
            DeliveryCompletionRule::Unrestricted
        );
        assert!(!WiringContract::UNRESTRICTED.requires_aggregate_ack());
        assert!(!WiringContract::unrestricted().requires_aggregate_ack());
        assert!(!WiringContract::at_most_per_output(1).requires_aggregate_ack());
        assert_eq!(
            WiringContract::UNRESTRICTED.aggregate_ack_propagation,
            AggregateAckPropagation::Unproven
        );

        let contract = WiringContract::unrestricted().requiring_aggregate_ack();
        assert!(contract.requires_aggregate_ack());
        assert_eq!(contract.output_fanout, OutputFanoutRule::Unrestricted);
        assert_eq!(
            contract.aggregate_ack_propagation,
            AggregateAckPropagation::Unproven
        );

        let capped = WiringContract::at_most_per_output(2).requiring_aggregate_ack();
        assert!(capped.requires_aggregate_ack());
        assert_eq!(capped.output_fanout, OutputFanoutRule::AtMostPerOutput(2));
    }

    /// Scenario: processor contracts declare unconditional or config-gated
    /// aggregate-Ack propagation.
    /// Guarantees: unproven processors fail closed, while a gated declaration
    /// is accepted only for the exact configured value.
    #[test]
    fn aggregate_ack_propagation_requires_an_explicit_matching_contract() {
        assert!(
            WiringContract::UNRESTRICTED
                .validate_aggregate_ack_propagation(&json!({}))
                .is_err()
        );

        let guaranteed = WiringContract::UNRESTRICTED.propagating_aggregate_ack();
        guaranteed
            .validate_aggregate_ack_propagation(&json!({}))
            .expect("unconditional propagation should be accepted");

        let gated = WiringContract::UNRESTRICTED
            .propagating_aggregate_ack_when_config_equals("/await_ack", "all");
        gated
            .validate_aggregate_ack_propagation(&json!({"await_ack": "all"}))
            .expect("matching propagation config should be accepted");
        assert!(
            gated
                .validate_aggregate_ack_propagation(&json!({"await_ack": "none"}))
                .is_err()
        );
        assert!(
            gated
                .validate_aggregate_ack_propagation(&json!({}))
                .is_err()
        );
    }

    /// Scenario: a `WiringContract` value is copied and compared.
    /// Guarantees: the contract remains `Copy`, `Clone`, and structurally
    /// comparable after gaining the delivery-completion field, so factory
    /// statics keep working unchanged.
    #[test]
    fn contract_remains_copy_clone_and_comparable() {
        fn takes_by_value(contract: WiringContract) -> WiringContract {
            contract
        }

        let contract = WiringContract::unrestricted().requiring_aggregate_ack();
        let copied = contract;
        let returned = takes_by_value(contract);
        assert_eq!(contract, copied);
        assert_eq!(contract, returned);
        assert_ne!(contract, WiringContract::UNRESTRICTED);

        let propagating = WiringContract::unrestricted().propagating_aggregate_ack();
        assert_eq!(propagating, takes_by_value(propagating));
    }
}
