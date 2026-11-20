use std::{collections::BTreeMap, sync::LazyLock};

use cortexkit_role_tool_display::notice_view_key;
use cortexkit_role_tool_provider::views::{NoticeView, NoticeViewDefinition, NoticeViewTemplates};
use serde_json::{json, Value};

use super::Kind;
use crate::subc::tool_views::current::CATALOG_DIGEST;

static VIEWS: LazyLock<BTreeMap<String, NoticeView>> = LazyLock::new(|| {
    let schema: Value = serde_json::from_str(include_str!("data.schema.json"))
        .expect("committed notice data schema");
    // Share the approved command layout across reminder, completion and watch
    // views, so an exit watch and a completion render the same status.
    let shared = include_str!("status.ui");
    let templates = NoticeViewTemplates::new(
        template("tool_row", shared, include_str!("row.ui")),
        template("tool_page", shared, include_str!("page.ui")),
    );
    Kind::ALL
        .into_iter()
        .map(|kind| {
            let pin =
                notice_view_key("aft", kind.name(), Kind::VERSION).expect("shipped notice pin");
            let view = NoticeView::new(
                NoticeViewDefinition::new(kind.name(), Kind::VERSION, schema.clone()),
                templates.clone(),
                1,
            );
            view.check_pin("aft", &pin).expect("shipped notice view");
            (pin, view)
        })
        .collect()
});

fn template(profile: &str, shared: &str, source: &str) -> Value {
    json!({
        "schema":"occipital.template/1",
        "profile":profile,
        "executionDialect":"1.0",
        "catalog":{"version":"1.1","digest":CATALOG_DIGEST},
        "reads":[],
        "source":format!("{shared}\n{source}"),
    })
}

pub(in crate::subc) fn view_pins() -> impl Iterator<Item = &'static String> {
    VIEWS.keys()
}

pub(in crate::subc) fn view(pin: &str) -> Option<NoticeView> {
    VIEWS.get(pin).cloned()
}
