use std::path::{Path, PathBuf};

pub use youth_state::{AppId, StateLocation};

use crate::RuntimeLimits;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct YouthAppConfig {
    pub component_path: PathBuf,
    pub app_id: AppId,
    pub state: StateLocation,
    pub limits: RuntimeLimits,
}

impl YouthAppConfig {
    pub fn ephemeral(component_path: impl AsRef<Path>) -> Self {
        Self {
            component_path: component_path.as_ref().to_owned(),
            app_id: AppId::parse("dev.youth.ephemeral")
                .expect("the built-in ephemeral application ID is valid"),
            state: StateLocation::Memory,
            limits: RuntimeLimits::default(),
        }
    }
}
