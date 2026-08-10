//! Broker instance routing for service definitions.

use crate::broker::lifecycle::names::{
    explicit_instance_pipe, private_broker_pipe, shared_broker_pipe, validate_service_name,
    PipePath, PipePathError,
};
use crate::broker::protocol::{BrokerIsolation, ServiceDefinition};

/// Stable key for one broker trust-domain instance.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BrokerInstanceKey {
    /// Per-user shared broker.
    Shared,
    /// Per-service private broker.
    Private {
        /// Service isolated into its own broker.
        service_name: String,
    },
    /// Explicit named broker instance.
    Explicit {
        /// Explicit trust-domain name.
        name: String,
    },
}

impl BrokerInstanceKey {
    /// Resolve a service definition into its broker instance key.
    pub fn from_service_definition(
        definition: &ServiceDefinition,
    ) -> Result<Self, BrokerInstanceError> {
        validate_service_name(&definition.service_name)?;
        match BrokerIsolation::try_from(definition.isolation) {
            Ok(BrokerIsolation::PrivateBroker) => Ok(Self::Private {
                service_name: definition.service_name.clone(),
            }),
            Ok(BrokerIsolation::SharedBroker) => Ok(Self::Shared),
            Ok(BrokerIsolation::ExplicitInstance) => {
                if definition.explicit_instance.is_empty() {
                    return Err(BrokerInstanceError::InvalidIsolation {
                        reason: "EXPLICIT_INSTANCE requires explicit_instance",
                    });
                }
                validate_service_name(&definition.explicit_instance)?;
                Ok(Self::Explicit {
                    name: definition.explicit_instance.clone(),
                })
            }
            Err(_) => Err(BrokerInstanceError::InvalidIsolation {
                reason: "unknown BrokerIsolation value",
            }),
        }
    }

    /// Stable manifest/debug identifier for this instance.
    pub fn id(&self) -> String {
        match self {
            Self::Shared => "shared".into(),
            Self::Private { service_name } => format!("private:{service_name}"),
            Self::Explicit { name } => format!("explicit:{name}"),
        }
    }

    /// Compute the broker pipe/socket for this instance and user.
    pub fn pipe_path(&self, user_sid_hash: &str) -> Result<PipePath, BrokerInstanceError> {
        match self {
            Self::Shared => shared_broker_pipe(user_sid_hash),
            Self::Private { service_name } => private_broker_pipe(user_sid_hash, service_name),
            Self::Explicit { name } => explicit_instance_pipe(user_sid_hash, name),
        }
        .map_err(BrokerInstanceError::PipePath)
    }
}

/// Errors returned while deriving broker instance identity.
#[derive(Debug, thiserror::Error)]
pub enum BrokerInstanceError {
    /// Pipe-name validation failed.
    #[error(transparent)]
    PipePath(#[from] PipePathError),
    /// The service definition used an inconsistent isolation shape.
    #[error("broker instance isolation is invalid: {reason}")]
    InvalidIsolation {
        /// Why the isolation shape was invalid.
        reason: &'static str,
    },
}
