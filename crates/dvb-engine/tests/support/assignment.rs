use dvb_core::{Effort, HireId, RoleId, RunAssignment, RunId, TaskId, Vendor};

pub struct Assignment(RunAssignment);

impl Assignment {
    pub fn from_assignment(assignment: RunAssignment) -> Self {
        Self(assignment)
    }

    pub fn run(mut self, run: RunId) -> Self {
        self.0.run = run;
        self
    }

    pub fn review_stage(mut self, stage: Option<dvb_core::roles::ReviewKind>) -> Self {
        self.0.review_stage = stage;
        self
    }

    pub fn new(run: RunId) -> Self {
        Self(RunAssignment {
            run,
            task: None,
            role: RoleId::new("backend"),
            hire: HireId::new("h"),
            model: "fake-1".into(),
            vendor: Vendor::Fake,
            effort: Effort::Medium,
            reason: "test".into(),
            reviews: None,
            base: None,
            review_stage: None,
            goal_version: None,
            routine: false,
        })
    }

    pub fn base(mut self, value: Option<String>) -> Self {
        self.0.base = value;
        self
    }

    pub fn effort(mut self, value: Effort) -> Self {
        self.0.effort = value;
        self
    }

    pub fn goal_version(mut self, value: Option<dvb_core::EventId>) -> Self {
        self.0.goal_version = value;
        self
    }

    pub fn hire(mut self, value: HireId) -> Self {
        self.0.hire = value;
        self
    }

    pub fn model(mut self, value: String) -> Self {
        self.0.model = value;
        self
    }

    pub fn reason(mut self, value: String) -> Self {
        self.0.reason = value;
        self
    }

    pub fn reviews(mut self, value: Option<String>) -> Self {
        self.0.reviews = value;
        self
    }

    pub fn role(mut self, value: RoleId) -> Self {
        self.0.role = value;
        self
    }

    pub fn task(mut self, value: Option<TaskId>) -> Self {
        self.0.task = value;
        self
    }

    pub fn vendor(mut self, value: Vendor) -> Self {
        self.0.vendor = value;
        self
    }

    pub fn build(self) -> RunAssignment {
        self.0
    }
}
