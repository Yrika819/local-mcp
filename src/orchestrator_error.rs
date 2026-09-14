use std::fmt;

#[derive(Debug)]
pub(crate) enum OrchestratorError {
    GoalNotFound,
    ActiveGoalAlreadyExists,
    InvalidTransition {
        entity: &'static str,
        from: String,
        to: String,
        reason: String,
    },
    InvalidDag(String),
    CorruptGoal(String),
    UnsupportedSchema(u64),
    SchemaUpgradeRequired(u64),
    RevisionConflict {
        expected: u64,
        actual: u64,
    },
    UnsafeIdentifier(String),
    PersistenceIo(std::io::Error),
    Serialization(serde_json::Error),
    RecoveryBlocked(String),
}

impl OrchestratorError {
    pub(crate) fn invalid_transition(
        entity: &'static str,
        from: impl Into<String>,
        to: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self::InvalidTransition {
            entity,
            from: from.into(),
            to: to.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for OrchestratorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GoalNotFound => write!(f, "goal was not found"),
            Self::ActiveGoalAlreadyExists => {
                write!(f, "a non-terminal goal already exists for this session")
            }
            Self::InvalidTransition {
                entity,
                from,
                to,
                reason,
            } => write!(
                f,
                "invalid {entity} transition {from} -> {to}: {reason}"
            ),
            Self::InvalidDag(reason) => write!(f, "invalid task DAG: {reason}"),
            Self::CorruptGoal(reason) => write!(f, "goal state is corrupt: {reason}"),
            Self::UnsupportedSchema(version) => {
                write!(f, "unsupported goal schema version {version}")
            }
            Self::SchemaUpgradeRequired(version) => {
                write!(f, "SCHEMA_UPGRADE_REQUIRED: goal schema version {version} requires explicit authority upgrade")
            }
            Self::RevisionConflict { expected, actual } => write!(
                f,
                "goal revision conflict: expected {expected}, current revision is {actual}"
            ),
            Self::UnsafeIdentifier(kind) => write!(f, "unsafe {kind} identifier"),
            Self::PersistenceIo(error) => write!(f, "goal persistence I/O failed: {error}"),
            Self::Serialization(error) => write!(f, "goal serialization failed: {error}"),
            Self::RecoveryBlocked(reason) => write!(f, "goal recovery is blocked: {reason}"),
        }
    }
}

impl std::error::Error for OrchestratorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PersistenceIo(error) => Some(error),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for OrchestratorError {
    fn from(value: std::io::Error) -> Self {
        Self::PersistenceIo(value)
    }
}

impl From<serde_json::Error> for OrchestratorError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value)
    }
}
