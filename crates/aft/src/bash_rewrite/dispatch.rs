use crate::bash_rewrite::rules::{
    CatAppendRule, CatRule, FindRule, GrepRule, LsRule, RgRule, SedRule,
};
use crate::bash_rewrite::RewriteRule;
use crate::context::AppContext;
use crate::protocol::Response;

pub fn dispatch(command: &str, session_id: Option<&str>, ctx: &AppContext) -> Option<Response> {
    if !ctx.config().experimental_bash_rewrite {
        return None;
    }

    let rules: [&dyn RewriteRule; 7] = [
        &GrepRule,
        &RgRule,
        &FindRule,
        &CatRule,
        &CatAppendRule,
        &SedRule,
        &LsRule,
    ];

    for rule in rules {
        if rule.matches(command) {
            match rule.rewrite(command, session_id, ctx) {
                Ok(response) => return Some(response),
                Err(message) => {
                    crate::slog_warn!("bash rewrite rule {} declined: {}", rule.name(), message);
                    return None;
                }
            }
        }
    }

    None
}
