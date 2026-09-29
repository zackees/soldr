//! The two chains `execute.rs` supervises side by side: the serial Dylint
//! branch, and the peer chain whose per-stage hooks scope Nextest
//! execution's lease and memory monitor (soldr#3446).

use super::*;

pub(super) trait DylintBranchVerifier {
    fn libraries_complete(&self) -> Result<(), SoldrError>;
    fn analysis_complete(&self) -> Result<(), SoldrError>;
    fn ui_tests_complete(&self) -> Result<(), SoldrError>;
}

pub(super) struct PlanDylintVerifier<'a>(pub(super) &'a CiTestPlan);

impl DylintBranchVerifier for PlanDylintVerifier<'_> {
    fn libraries_complete(&self) -> Result<(), SoldrError> {
        verify_target_tree("Dylint library", &self.0.dylint_target_trees.libraries)
    }

    fn analysis_complete(&self) -> Result<(), SoldrError> {
        verify_target_tree("Dylint analysis", &self.0.dylint_target_trees.analysis)
    }

    fn ui_tests_complete(&self) -> Result<(), SoldrError> {
        verify_dylint_test_targets(self.0)
    }
}

#[derive(Clone, Copy)]
pub(super) enum DylintPhase {
    Library(usize),
    Workspace,
    UiTest(usize),
    Complete,
}

pub(super) struct DylintBranch<'a> {
    pub(super) libraries: Vec<&'a Stage>,
    pub(super) workspace: Option<&'a Stage>,
    pub(super) ui_tests: Vec<&'a Stage>,
    pub(super) phase: DylintPhase,
    /// Every `dylint-library-*` stage exited successfully in this run, so the
    /// library marker may record them (`dylint_library_marker::finish`).
    pub(super) libraries_built: bool,
}

impl<'a> DylintBranch<'a> {
    pub(super) fn from_plan(plan: &'a CiTestPlan) -> Result<Self, SoldrError> {
        let ui_tests: Vec<_> = plan
            .stages
            .iter()
            .filter(|stage| stage.name.starts_with("dylint-test-"))
            .collect();
        Self::new(ui_tests)
    }

    pub(super) fn compilation(
        libraries: Vec<&'a Stage>,
        workspace: &'a Stage,
    ) -> Result<Self, SoldrError> {
        if libraries.is_empty() {
            return Err(SoldrError::Other(
                "soldr ci-test: parallel Dylint compilation branch has no libraries".into(),
            ));
        }
        Ok(Self {
            libraries,
            workspace: Some(workspace),
            ui_tests: Vec::new(),
            phase: DylintPhase::Library(0),
            libraries_built: false,
        })
    }

    /// The whole serial Dylint branch -- libraries, workspace analysis, then
    /// UI tests -- supervised as one chain beside Nextest (soldr#3446).
    pub(super) fn full_from_plan(
        plan: &'a CiTestPlan,
        skip_libraries: bool,
        verifier: &impl DylintBranchVerifier,
    ) -> Result<Self, SoldrError> {
        let mut branch = Self::compilation_from_plan(plan, skip_libraries, verifier)?;
        branch.ui_tests = Self::from_plan(plan)?.ui_tests;
        Ok(branch)
    }

    /// `skip_libraries` (soldr#2349) fast-forwards to `Workspace`, skipping
    /// the six `dylint-library-*` stages; `libraries_complete()` still runs
    /// eagerly as a safety net against the tree being wiped meanwhile.
    pub(super) fn compilation_from_plan(
        plan: &'a CiTestPlan,
        skip_libraries: bool,
        verifier: &impl DylintBranchVerifier,
    ) -> Result<Self, SoldrError> {
        let libraries = plan
            .stages
            .iter()
            .filter(|stage| stage.name.starts_with("dylint-library-"))
            .collect();
        let mut branch = Self::compilation(libraries, stage_named(plan, "dylint-workspace")?)?;
        if skip_libraries {
            let names: Vec<&str> = branch.libraries.iter().map(|s| s.name.as_str()).collect();
            dylint_library_marker::announce_skip(&names);
            verifier.libraries_complete()?;
            branch.phase = DylintPhase::Workspace;
        }
        Ok(branch)
    }

    pub(super) fn new(ui_tests: Vec<&'a Stage>) -> Result<Self, SoldrError> {
        if ui_tests.is_empty() {
            return Err(SoldrError::Other(
                "soldr ci-test: parallel Dylint UI-test branch is empty".into(),
            ));
        }
        Ok(Self {
            libraries: Vec::new(),
            workspace: None,
            ui_tests,
            phase: DylintPhase::UiTest(0),
            libraries_built: false,
        })
    }

    pub(super) fn current(&self) -> Option<&'a Stage> {
        match self.phase {
            DylintPhase::Library(index) => self.libraries.get(index).copied(),
            DylintPhase::Workspace => self.workspace,
            DylintPhase::UiTest(index) => self.ui_tests.get(index).copied(),
            DylintPhase::Complete => None,
        }
    }

    pub(super) fn advance(
        &mut self,
        verifier: &impl DylintBranchVerifier,
    ) -> Result<Option<&'a Stage>, SoldrError> {
        match self.phase {
            DylintPhase::Library(index) if index + 1 < self.libraries.len() => {
                self.phase = DylintPhase::Library(index + 1);
            }
            DylintPhase::Library(_) => {
                verifier.libraries_complete()?;
                self.libraries_built = true;
                self.phase = DylintPhase::Workspace;
            }
            DylintPhase::Workspace => {
                verifier.analysis_complete()?;
                self.phase = if self.ui_tests.is_empty() {
                    DylintPhase::Complete
                } else {
                    DylintPhase::UiTest(0)
                };
            }
            DylintPhase::UiTest(index) if index + 1 < self.ui_tests.len() => {
                self.phase = DylintPhase::UiTest(index + 1);
            }
            DylintPhase::UiTest(_) => {
                verifier.ui_tests_complete()?;
                self.phase = DylintPhase::Complete;
            }
            DylintPhase::Complete => {}
        }
        Ok(self.current())
    }
}

/// Per-stage lifecycle hooks for the peer chain supervised beside Dylint.
///
/// `before_spawn` runs immediately before a peer stage starts and may refuse
/// it; `after_exit` runs once that stage has exited. Resources scoped to one
/// stage (Nextest execution's resident lease and memory monitor) attach here
/// so they cover exactly that stage's lifetime, not the whole chain's.
pub(super) trait PeerStageHooks {
    fn before_spawn(&mut self, stage: &Stage) -> Result<(), SoldrError>;
    fn after_exit(&mut self, stage: &Stage);
}

pub(super) struct NoPeerHooks;

impl PeerStageHooks for NoPeerHooks {
    fn before_spawn(&mut self, _stage: &Stage) -> Result<(), SoldrError> {
        Ok(())
    }

    fn after_exit(&mut self, _stage: &Stage) {}
}

/// Attaches Nextest EXECUTION's stage-scoped resources to the `nextest`
/// stage of the peer chain: soldr#2885's memory monitor, which the wrapper
/// around each Unix test obeys, and soldr#2878's daemon resident-capacity
/// lease. Both start immediately before that stage spawns and end the moment
/// it exits, so `nextest-compile` never holds either.
pub(super) struct NextestExecutionHooks<'f, C: nextest_resident_lease::ResidentLeaseController> {
    pub(super) admission: &'f super::super::test_pressure::NextestAdmission,
    pub(super) lease_controller: C,
    pub(super) lease: Option<Option<C::Lease>>,
    pub(super) pressure: Option<super::super::test_pressure::PressureMonitor>,
}

impl<C: nextest_resident_lease::ResidentLeaseController> PeerStageHooks
    for NextestExecutionHooks<'_, C>
{
    fn before_spawn(&mut self, stage: &Stage) -> Result<(), SoldrError> {
        if stage.name != "nextest" {
            return Ok(());
        }
        self.pressure = Some(self.admission.start()?);
        self.lease = nextest_resident_lease::acquire_for_stage(&self.lease_controller, &stage.name);
        Ok(())
    }

    fn after_exit(&mut self, stage: &Stage) {
        if stage.name != "nextest" {
            return;
        }
        if let Some(lease) = self.lease.take() {
            self.lease_controller.release(lease);
        }
        if let Some(pressure) = self.pressure.take() {
            pressure.finish();
        }
    }
}
