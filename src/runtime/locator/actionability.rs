use serde::Deserialize;

use crate::runtime::{BrowserError, LocatorFailure, OperationPhase};

#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct ActionabilityFacts {
    pub(crate) attached: bool,
    pub(crate) visible: bool,
    pub(crate) enabled: bool,
    pub(crate) stable: bool,
    pub(crate) obscured: bool,
    #[serde(default)]
    pub(crate) editable: bool,
    #[serde(default)]
    pub(crate) checkable: bool,
    #[serde(default)]
    pub(crate) radio: bool,
    #[serde(default)]
    pub(crate) selectable: bool,
    #[serde(default)]
    pub(crate) file_input: bool,
    #[serde(default)]
    pub(crate) checked: bool,
}

impl ActionabilityFacts {
    pub(crate) fn ensure_actionable(self) -> Result<(), BrowserError> {
        let failure = if !self.attached {
            LocatorFailure::NotFound
        } else if !self.visible {
            LocatorFailure::NotVisible
        } else if !self.enabled {
            LocatorFailure::Disabled
        } else if !self.stable {
            LocatorFailure::Unstable
        } else if self.obscured {
            LocatorFailure::Obscured
        } else {
            return Ok(());
        };
        let message = match failure {
            LocatorFailure::NotFound => "locator element is detached",
            LocatorFailure::NotVisible => "locator element is not visible",
            LocatorFailure::Disabled => "locator element is disabled",
            LocatorFailure::Unstable => "locator element is not stable",
            LocatorFailure::Obscured => "locator element is obscured",
            LocatorFailure::NotEditable
            | LocatorFailure::NotCheckable
            | LocatorFailure::NotUncheckable
            | LocatorFailure::NotSelectable
            | LocatorFailure::NotFileInput => unreachable!(),
            LocatorFailure::Ambiguous { .. } => unreachable!(),
        };
        Err(
            BrowserError::operation("check locator actionability", OperationPhase::Observation)
                .with_message(message)
                .with_locator_failure(failure),
        )
    }

    pub(crate) fn ensure_editable(self) -> Result<(), BrowserError> {
        self.ensure_actionable()?;
        self.ensure_kind(
            self.editable,
            LocatorFailure::NotEditable,
            "locator element is not editable",
        )
    }

    pub(crate) fn ensure_checkable(self) -> Result<(), BrowserError> {
        self.ensure_actionable()?;
        self.ensure_kind(
            self.checkable,
            LocatorFailure::NotCheckable,
            "locator element is not checkable",
        )
    }

    pub(crate) fn ensure_uncheckable(self) -> Result<(), BrowserError> {
        self.ensure_checkable()?;
        self.ensure_kind(
            !self.radio,
            LocatorFailure::NotUncheckable,
            "radio controls cannot be unchecked directly",
        )
    }

    pub(crate) fn ensure_selectable(self) -> Result<(), BrowserError> {
        self.ensure_actionable()?;
        self.ensure_kind(
            self.selectable,
            LocatorFailure::NotSelectable,
            "locator element is not a select control",
        )
    }

    pub(crate) fn ensure_file_input(self) -> Result<(), BrowserError> {
        if !self.attached {
            return self.ensure_actionable();
        }
        self.ensure_kind(
            self.file_input,
            LocatorFailure::NotFileInput,
            "locator element is not a file input",
        )
    }

    fn ensure_kind(
        self,
        condition: bool,
        failure: LocatorFailure,
        message: &'static str,
    ) -> Result<(), BrowserError> {
        if condition {
            Ok(())
        } else {
            Err(
                BrowserError::operation("check locator actionability", OperationPhase::Observation)
                    .with_message(message)
                    .with_locator_failure(failure),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::LocatorFailure;

    fn facts() -> ActionabilityFacts {
        ActionabilityFacts {
            attached: true,
            visible: true,
            enabled: true,
            stable: true,
            obscured: false,
            editable: false,
            checkable: false,
            radio: false,
            selectable: false,
            file_input: false,
            checked: false,
        }
    }

    #[test]
    fn invisible_element_has_a_structured_failure() {
        let error = ActionabilityFacts {
            visible: false,
            ..facts()
        }
        .ensure_actionable()
        .unwrap_err();
        assert_eq!(error.locator_failure(), Some(&LocatorFailure::NotVisible));
    }

    #[test]
    fn disabled_element_has_a_structured_failure() {
        let error = ActionabilityFacts {
            enabled: false,
            ..facts()
        }
        .ensure_actionable()
        .unwrap_err();
        assert_eq!(error.locator_failure(), Some(&LocatorFailure::Disabled));
    }

    #[test]
    fn unstable_element_has_a_structured_failure() {
        let error = ActionabilityFacts {
            stable: false,
            ..facts()
        }
        .ensure_actionable()
        .unwrap_err();
        assert_eq!(error.locator_failure(), Some(&LocatorFailure::Unstable));
    }

    #[test]
    fn obscured_element_has_a_structured_failure() {
        let error = ActionabilityFacts {
            obscured: true,
            ..facts()
        }
        .ensure_actionable()
        .unwrap_err();
        assert_eq!(error.locator_failure(), Some(&LocatorFailure::Obscured));
    }

    #[test]
    fn action_specific_gates_are_structured_and_do_not_conflate_element_kinds() {
        let not_editable = facts().ensure_editable().unwrap_err();
        assert_eq!(
            not_editable.locator_failure(),
            Some(&LocatorFailure::NotEditable)
        );

        let not_checkable = facts().ensure_checkable().unwrap_err();
        assert_eq!(
            not_checkable.locator_failure(),
            Some(&LocatorFailure::NotCheckable)
        );

        let not_selectable = facts().ensure_selectable().unwrap_err();
        assert_eq!(
            not_selectable.locator_failure(),
            Some(&LocatorFailure::NotSelectable)
        );

        let not_file_input = facts().ensure_file_input().unwrap_err();
        assert_eq!(
            not_file_input.locator_failure(),
            Some(&LocatorFailure::NotFileInput)
        );

        let not_uncheckable = ActionabilityFacts {
            checkable: true,
            radio: true,
            ..facts()
        }
        .ensure_uncheckable()
        .unwrap_err();
        assert_eq!(
            not_uncheckable.locator_failure(),
            Some(&LocatorFailure::NotUncheckable)
        );
    }
}
