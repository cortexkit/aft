use super::*;

impl<'ctx> ToolLaunchPolicy<'ctx> {
    pub(super) fn remote_edit_refusal() -> Self {
        Self {
            ctx: None,
            request: std::rc::Rc::new(RequestToolLaunch::resolved(ResolvedToolLaunch {
                plan: SpawnPlan::Refused {
                    code: "sandbox_unavailable",
                    message: "remote file edits cannot launch external tools".into(),
                    mismatch_class: None,
                },
                environment: HashMap::new(),
                task: None,
            })),
        }
    }
}
